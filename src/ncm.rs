use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use crc32fast::Hasher;
use serde::Deserialize;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const CORE_KEY: &[u8; 16] = b"hzHRAmso5kInbaxW";
const META_KEY: &[u8; 16] = b"#14ljk_!\\]&0U<'(";
const KEY_PREFIX: &[u8] = b"neteasecloudmusic";
const META_PREFIX: &[u8] = b"163 key(Don't modify):";
const MAX_KEY_SIZE: u32 = 1024 * 1024;
const MAX_META_SIZE: u32 = 8 * 1024 * 1024;
const MAX_COVER_SIZE: u32 = 32 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackMetadata {
    pub format: Option<String>,
    pub music_name: Option<String>,
    pub music_id: Option<Value>,
    pub album: Option<String>,
    pub artist: Option<Vec<Vec<Value>>>,
    pub track: Option<u32>,
}

impl TrackMetadata {
    pub fn music_id(&self) -> Option<String> {
        let id = match self.music_id.as_ref()? {
            Value::String(id) => id.clone(),
            Value::Number(id) => id.to_string(),
            _ => return None,
        };
        (!id.is_empty()
            && id.len() <= 20
            && id != "0"
            && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(id)
    }
}

pub struct NcmAudio {
    file: File,
    key_mask: [u8; 256],
    audio_start: u64,
    audio_len: u64,
    format: String,
    metadata: TrackMetadata,
    cover: Vec<u8>,
}

impl NcmAudio {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
        let file_len = file.metadata()?.len();

        let mut magic = [0u8; 8];
        file.read_exact(&mut magic).context("NCM 文件头不完整")?;
        ensure!(&magic == b"CTENFDAM", "不是有效的 NCM 文件");
        file.seek(SeekFrom::Current(2))?;

        let mut encrypted_key = read_block(&mut file, MAX_KEY_SIZE, "密钥")?;
        ensure!(!encrypted_key.is_empty(), "NCM 密钥块为空");
        for byte in &mut encrypted_key {
            *byte ^= 0x64;
        }
        aes_ecb_decrypt(&mut encrypted_key, CORE_KEY).context("NCM 密钥解密失败")?;
        unpad_pkcs7(&mut encrypted_key).context("NCM 密钥填充无效")?;
        ensure!(
            encrypted_key.starts_with(KEY_PREFIX) && encrypted_key.len() > KEY_PREFIX.len(),
            "NCM 音频密钥无效"
        );
        let key_mask = build_key_mask(&encrypted_key[KEY_PREFIX.len()..])?;

        let mut encrypted_meta = read_block(&mut file, MAX_META_SIZE, "元数据")?;
        let metadata = if encrypted_meta.is_empty() {
            TrackMetadata::default()
        } else {
            for byte in &mut encrypted_meta {
                *byte ^= 0x63;
            }
            ensure!(
                encrypted_meta.starts_with(META_PREFIX),
                "NCM 元数据标记无效"
            );
            let mut meta_cipher = STANDARD
                .decode(&encrypted_meta[META_PREFIX.len()..])
                .context("NCM 元数据 Base64 无效")?;
            aes_ecb_decrypt(&mut meta_cipher, META_KEY).context("NCM 元数据解密失败")?;
            unpad_pkcs7(&mut meta_cipher).context("NCM 元数据填充无效")?;
            ensure!(meta_cipher.starts_with(b"music:"), "NCM 元数据内容无效");
            serde_json::from_slice(&meta_cipher[6..]).context("NCM 歌曲信息 JSON 无效")?
        };

        // The CRC covers every byte before it; the next byte is the cover version.
        let crc_position = file.stream_position()?;
        let expected_crc = read_u32(&mut file, "CRC32")?;
        let mut _cover_version = [0u8; 1];
        file.read_exact(&mut _cover_version)
            .context("NCM 封面版本不完整")?;
        verify_prefix_crc(&mut file, crc_position, expected_crc)?;
        let cover_frame_len = read_u32(&mut file, "封面帧长度")?;
        let cover_len = read_u32(&mut file, "封面长度")?;
        ensure!(cover_len <= cover_frame_len, "NCM 封面长度异常");
        ensure!(cover_frame_len <= MAX_COVER_SIZE, "NCM 封面帧过大");
        let cover_start = file.stream_position()?;
        ensure!(
            u64::from(cover_frame_len) <= file_len.saturating_sub(cover_start),
            "NCM 封面区不完整"
        );
        let mut cover = vec![0u8; usize::try_from(cover_len)?];
        file.read_exact(&mut cover).context("NCM 封面数据不完整")?;
        file.seek(SeekFrom::Current(i64::from(cover_frame_len - cover_len)))?;

        let audio_start = file.stream_position()?;
        let audio_len = file_len.saturating_sub(audio_start);
        ensure!(audio_len >= 4, "NCM 文件缺少音频数据");
        let detected_format = detect_format(&mut file, audio_start, audio_len, &key_mask)?;
        let format = match metadata.format.as_deref() {
            Some(declared) if !declared.trim().is_empty() => {
                let declared = normalize_format(declared)?;
                if let Some(detected) = detected_format.as_deref() {
                    ensure!(
                        formats_match(&declared, detected),
                        "元数据标记为 {declared}，但音频数据识别为 {detected}"
                    );
                }
                declared
            }
            _ => detected_format.context("无法从 NCM 元数据或音频头识别格式")?,
        };

        Ok(Self {
            file,
            key_mask,
            audio_start,
            audio_len,
            format,
            metadata,
            cover,
        })
    }

    pub fn format(&self) -> &str {
        &self.format
    }

    pub fn metadata(&self) -> &TrackMetadata {
        &self.metadata
    }

    pub fn cover(&self) -> &[u8] {
        &self.cover
    }

    pub fn copy_decoded_to<W: Write>(&mut self, output: &mut W) -> Result<()> {
        self.file.seek(SeekFrom::Start(self.audio_start))?;
        let mut buffer = [0u8; 64 * 1024];
        let mut offset = 0u64;
        while offset < self.audio_len {
            let chunk_len = usize::try_from((self.audio_len - offset).min(buffer.len() as u64))?;
            self.file.read_exact(&mut buffer[..chunk_len])?;
            for (index, byte) in buffer[..chunk_len].iter_mut().enumerate() {
                *byte ^= self.key_mask[((offset + index as u64) & 0xff) as usize];
            }
            output.write_all(&buffer[..chunk_len])?;
            offset += chunk_len as u64;
        }
        Ok(())
    }
}

fn read_u32(file: &mut File, label: &str) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)
        .with_context(|| format!("NCM {label}不完整"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn verify_prefix_crc(file: &mut File, prefix_len: u64, expected_crc: u32) -> Result<()> {
    let resume_position = file.stream_position()?;
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Hasher::new();
    let mut remaining = prefix_len;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..chunk_len])
            .context("读取 NCM CRC32 校验数据失败")?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    file.seek(SeekFrom::Start(resume_position))?;
    ensure!(hasher.finalize() == expected_crc, "NCM CRC32 校验失败");
    Ok(())
}

fn read_block(file: &mut File, max_len: u32, label: &str) -> Result<Vec<u8>> {
    let len = read_u32(file, &format!("{label}块长度"))?;
    ensure!(len <= max_len, "NCM {label}块超出大小限制");
    let remaining = file
        .metadata()?
        .len()
        .saturating_sub(file.stream_position()?);
    ensure!(u64::from(len) <= remaining, "NCM {label}块不完整");
    let mut data = vec![0u8; len as usize];
    file.read_exact(&mut data)
        .with_context(|| format!("读取 NCM {label}块失败"))?;
    Ok(data)
}

fn aes_ecb_decrypt(data: &mut [u8], key: &[u8; 16]) -> Result<()> {
    ensure!(!data.is_empty() && data.len() % 16 == 0, "AES 数据长度无效");
    let cipher = Aes128::new(key.into());
    for chunk in data.chunks_exact_mut(16) {
        let block = aes::cipher::Block::<Aes128>::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }
    Ok(())
}

fn unpad_pkcs7(data: &mut Vec<u8>) -> Result<()> {
    let pad_len = usize::from(*data.last().context("AES 填充数据为空")?);
    ensure!((1..=16).contains(&pad_len), "AES 填充长度无效");
    ensure!(pad_len <= data.len(), "AES 填充长度超出数据范围");
    ensure!(
        data[data.len() - pad_len..]
            .iter()
            .all(|byte| usize::from(*byte) == pad_len),
        "AES 填充内容无效"
    );
    data.truncate(data.len() - pad_len);
    Ok(())
}

fn build_key_mask(key_data: &[u8]) -> Result<[u8; 256]> {
    ensure!(!key_data.is_empty(), "NCM 密钥数据为空");
    let mut key_box = std::array::from_fn::<_, 256, _>(|index| index as u8);
    let mut last_byte = 0usize;
    for index in 0..256 {
        let swap = key_box[index];
        let c =
            (usize::from(swap) + last_byte + usize::from(key_data[index % key_data.len()])) & 0xff;
        key_box[index] = key_box[c];
        key_box[c] = swap;
        last_byte = c;
    }

    let mut mask = [0u8; 256];
    for (index, byte) in mask.iter_mut().enumerate() {
        let j = (index + 1) & 0xff;
        let lookup = (usize::from(key_box[j])
            + usize::from(key_box[(usize::from(key_box[j]) + j) & 0xff]))
            & 0xff;
        *byte = key_box[lookup];
    }
    Ok(mask)
}

fn detect_format(
    file: &mut File,
    audio_start: u64,
    audio_len: u64,
    mask: &[u8; 256],
) -> Result<Option<String>> {
    let mut header = [0u8; 16];
    let size = usize::try_from(audio_len.min(header.len() as u64))?;
    file.seek(SeekFrom::Start(audio_start))?;
    file.read_exact(&mut header[..size])?;
    for (index, byte) in header[..size].iter_mut().enumerate() {
        *byte ^= mask[index & 0xff];
    }

    let bytes = &header[..size];
    let format = if bytes.starts_with(b"fLaC") {
        Some("flac")
    } else if bytes.starts_with(b"ID3") || is_mp3_frame(bytes) {
        Some("mp3")
    } else if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        Some("m4a")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some("wav")
    } else if bytes.starts_with(b"OggS") {
        Some("ogg")
    } else if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0 {
        Some("aac")
    } else if bytes.starts_with(&[0x30, 0x26, 0xb2, 0x75, 0x8e, 0x66, 0xcf, 0x11]) {
        Some("wma")
    } else {
        None
    };
    Ok(format.map(str::to_owned))
}

fn is_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 3
        && bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && bytes[1] & 0x06 != 0
        && bytes[2] & 0xf0 != 0xf0
}

fn normalize_format(format: &str) -> Result<String> {
    let normalized = format.trim().trim_start_matches('.').to_ascii_lowercase();
    ensure!(
        matches!(
            normalized.as_str(),
            "flac" | "mp3" | "m4a" | "aac" | "wav" | "ogg" | "opus" | "wma"
        ),
        "不支持或不安全的 NCM 音频格式：{format}"
    );
    Ok(normalized)
}

fn formats_match(declared: &str, detected: &str) -> bool {
    declared == detected || (declared == "opus" && detected == "ogg")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let pad_len = 16 - data.len() % 16;
        let mut padded = data.to_vec();
        padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));
        let cipher = Aes128::new_from_slice(key).unwrap();
        for chunk in padded.chunks_exact_mut(16) {
            let block = aes::cipher::Block::<Aes128>::from_mut_slice(chunk);
            cipher.encrypt_block(block);
        }
        padded
    }

    fn write_block(output: &mut Vec<u8>, block: &[u8]) {
        output.extend_from_slice(&(block.len() as u32).to_le_bytes());
        output.extend_from_slice(block);
    }

    fn fixture() -> (Vec<u8>, Vec<u8>) {
        let key = b"synthetic-key";
        let mut key_plain = KEY_PREFIX.to_vec();
        key_plain.extend_from_slice(key);
        let mut encrypted_key = aes_ecb_encrypt(CORE_KEY, &key_plain);
        for byte in &mut encrypted_key {
            *byte ^= 0x64;
        }

        let json =
            br#"{"format":"flac","musicName":"fixture","album":"test","artist":[["artist",1]]}"#;
        let mut meta_plain = b"music:".to_vec();
        meta_plain.extend_from_slice(json);
        let encrypted_meta = aes_ecb_encrypt(META_KEY, &meta_plain);
        let mut meta_blob = META_PREFIX.to_vec();
        meta_blob.extend_from_slice(STANDARD.encode(encrypted_meta).as_bytes());
        for byte in &mut meta_blob {
            *byte ^= 0x63;
        }

        let audio = b"fLaC synthetic FLAC payload".to_vec();
        let mask = build_key_mask(key).unwrap();
        let mut encrypted_audio = audio.clone();
        for (index, byte) in encrypted_audio.iter_mut().enumerate() {
            *byte ^= mask[index & 0xff];
        }

        let cover = b"cover bytes";
        let additional_cover_data = b"secondary";
        let mut fixture = b"CTENFDAM\0\0".to_vec();
        write_block(&mut fixture, &encrypted_key);
        write_block(&mut fixture, &meta_blob);
        let crc = crc32fast::hash(&fixture);
        fixture.extend_from_slice(&crc.to_le_bytes());
        fixture.push(1);
        fixture
            .extend_from_slice(&((cover.len() + additional_cover_data.len()) as u32).to_le_bytes());
        fixture.extend_from_slice(&(cover.len() as u32).to_le_bytes());
        fixture.extend_from_slice(cover);
        fixture.extend_from_slice(additional_cover_data);
        fixture.extend_from_slice(&encrypted_audio);
        (fixture, audio)
    }

    fn checksum_offset(fixture: &[u8]) -> usize {
        let mut offset = 10;
        for _ in 0..2 {
            let block_len =
                u32::from_le_bytes(fixture[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4 + block_len;
        }
        offset
    }

    #[test]
    fn decrypts_ncm_and_reads_metadata() -> Result<()> {
        let (fixture, expected_audio) = fixture();
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&fixture)?;
        let mut ncm = NcmAudio::open(file.path())?;
        assert_eq!(ncm.format(), "flac");
        assert_eq!(ncm.metadata().music_name.as_deref(), Some("fixture"));
        assert_eq!(ncm.cover(), b"cover bytes");
        assert_eq!(
            ncm.metadata().artist.as_ref().unwrap()[0][0].as_str(),
            Some("artist")
        );
        let mut decoded = Vec::new();
        ncm.copy_decoded_to(&mut decoded)?;
        assert_eq!(decoded, expected_audio);
        Ok(())
    }

    #[test]
    fn rejects_corrupted_crc() -> Result<()> {
        let (mut fixture, _) = fixture();
        let crc_offset = checksum_offset(&fixture);
        fixture[crc_offset] ^= 1;
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(&fixture)?;
        assert!(NcmAudio::open(file.path()).is_err());
        Ok(())
    }

    #[test]
    fn parses_numeric_or_string_music_ids() -> Result<()> {
        let numeric: TrackMetadata = serde_json::from_str(r#"{"musicId":12345}"#)?;
        let string: TrackMetadata = serde_json::from_str(r#"{"musicId":"67890"}"#)?;
        let invalid: TrackMetadata = serde_json::from_str(r#"{"musicId":"not-an-id"}"#)?;
        assert_eq!(numeric.music_id().as_deref(), Some("12345"));
        assert_eq!(string.music_id().as_deref(), Some("67890"));
        assert_eq!(invalid.music_id(), None);
        Ok(())
    }

    #[test]
    fn rejects_invalid_header() -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(b"not an ncm file")?;
        assert!(NcmAudio::open(file.path()).is_err());
        Ok(())
    }

    #[test]
    fn mask_is_repeating_xor_stream() {
        let mask = build_key_mask(b"sample key").unwrap();
        let mut data = b"a short audio stream".to_vec();
        let original = data.clone();
        for (index, byte) in data.iter_mut().enumerate() {
            *byte ^= mask[index & 0xff];
        }
        for (index, byte) in data.iter_mut().enumerate() {
            *byte ^= mask[index & 0xff];
        }
        assert_eq!(data, original);
    }
}
