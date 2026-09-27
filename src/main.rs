mod ncm;

use anyhow::{bail, ensure, Context, Result};
use clap::{Parser, ValueEnum};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use lofty::tag::Tag;
use lyrics_fetcher::{save_for_track, LyricsResult};
use ncm::{NcmAudio, TrackMetadata};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Original,
    Flac,
    M4a,
    Mp4,
    Mp3,
}

impl OutputFormat {
    fn extension(self, original: &str) -> &str {
        match self {
            Self::Original => original,
            Self::Flac => "flac",
            Self::M4a => "m4a",
            Self::Mp4 => "mp4",
            Self::Mp3 => "mp3",
        }
    }

    fn needs_transcode(self, original: &str) -> bool {
        matches!(self, Self::M4a | Self::Mp4) || self.extension(original) != original
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AudioCodec {
    Alac,
    Aac,
    Mp3,
}

#[derive(Debug, PartialEq, Eq)]
struct FfmpegEncoding {
    muxer: &'static str,
    encoder: &'static str,
    quality_args: &'static [&'static str],
}

fn ffmpeg_encoding(format: OutputFormat, codec: AudioCodec) -> Result<FfmpegEncoding> {
    match format {
        OutputFormat::Flac => Ok(FfmpegEncoding {
            muxer: "flac",
            encoder: "flac",
            quality_args: &[],
        }),
        OutputFormat::Mp3 => Ok(FfmpegEncoding {
            muxer: "mp3",
            encoder: "libmp3lame",
            quality_args: &["-q:a", "2"],
        }),
        OutputFormat::M4a | OutputFormat::Mp4 => {
            let (encoder, quality_args) = match codec {
                AudioCodec::Alac => ("alac", &[][..]),
                AudioCodec::Aac => ("aac", &["-b:a", "192k"][..]),
                AudioCodec::Mp3 => ("libmp3lame", &["-q:a", "2"][..]),
            };
            Ok(FfmpegEncoding {
                muxer: "mp4",
                encoder,
                quality_args,
            })
        }
        OutputFormat::Original => bail!("original 格式不需要转码"),
    }
}

#[derive(Parser)]
#[command(
    name = "ncm-audio",
    version,
    about = "解密 NCM 音频；可选转码为 FLAC、M4A/MP4 或 MP3",
    after_help = "M4A/MP4 默认使用无损 ALAC；可用 --codec aac 或 --codec mp3 减小体积。FLAC 与 ALAC 无损，AAC 与 MP3 有损。跨格式转码需要本机安装 ffmpeg。"
)]
struct Args {
    #[arg(value_name = "INPUT")]
    inputs: Vec<PathBuf>,

    #[arg(short, long, value_enum, default_value = "original")]
    format: OutputFormat,

    #[arg(
        long,
        value_enum,
        default_value = "alac",
        help = "M4A/MP4 内部音频编码"
    )]
    codec: AudioCodec,

    #[arg(short, long, default_value = "output", value_name = "DIR")]
    output: PathBuf,

    #[arg(short, long, help = "递归扫描输入目录")]
    recursive: bool,

    #[arg(long, help = "优先使用同名 .lrc；没有时联网查询网易云")]
    lyrics: bool,

    #[arg(long, help = "覆盖同名输出文件")]
    force: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let inputs = if args.inputs.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        args.inputs.clone()
    };
    let mut files = Vec::new();
    for input in inputs {
        collect_ncm_files(&input, args.recursive, &mut files)?;
    }
    files.sort();
    files.dedup();
    ensure!(!files.is_empty(), "没有找到 .ncm 文件");

    let mut failures = 0usize;
    for path in &files {
        match convert(path, &args) {
            Ok(output) => println!("{} -> {}", path.display(), output.display()),
            Err(error) => {
                failures += 1;
                eprintln!("{}: {error:#}", path.display());
            }
        }
    }
    if failures > 0 {
        bail!("{failures}/{} 个文件转换失败", files.len());
    }
    Ok(())
}

fn collect_ncm_files(path: &Path, recursive: bool, files: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("无法访问输入路径 {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("不处理符号链接：{}", path.display());
    }
    if metadata.is_file() {
        if is_ncm(path) {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    ensure!(
        metadata.is_dir(),
        "输入路径不是文件或目录：{}",
        path.display()
    );
    for entry in fs::read_dir(path).with_context(|| format!("无法读取目录 {}", path.display()))?
    {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_file() && is_ncm(&entry.path()) {
            files.push(entry.path());
        } else if recursive && kind.is_dir() {
            collect_ncm_files(&entry.path(), true, files)?;
        }
    }
    Ok(())
}

fn is_ncm(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ncm"))
}

fn convert(source: &Path, args: &Args) -> Result<PathBuf> {
    let mut ncm = NcmAudio::open(source)?;
    let original_format = ncm.format().to_owned();
    let target_format = args.format.extension(&original_format).to_owned();
    let transcode = args.format.needs_transcode(&original_format);
    if transcode {
        ffmpeg_encoding(args.format, args.codec)?;
        let status = Command::new("ffmpeg")
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("跨格式转换需要 ffmpeg，请先安装并加入 PATH")?;
        ensure!(status.success(), "ffmpeg 版本检查失败");
    }

    let output_dir = &args.output;
    fs::create_dir_all(output_dir)
        .with_context(|| format!("无法创建输出目录 {}", output_dir.display()))?;
    let output_path = output_dir.join(format!(
        "{}.{}",
        output_stem(ncm.metadata(), source),
        target_format
    ));
    if !args.force {
        ensure!(
            !output_path.exists(),
            "输出文件已存在（如需覆盖请使用 --force）：{}",
            output_path.display()
        );
    }

    let mut decoded = tempfile::Builder::new()
        .prefix(".ncm-decoded-")
        .suffix(&format!(".{original_format}"))
        .tempfile_in(output_dir)
        .context("无法创建解密临时文件")?;
    ncm.copy_decoded_to(decoded.as_file_mut())?;
    decoded.as_file_mut().sync_all()?;

    if transcode {
        let mut converted = tempfile::Builder::new()
            .prefix(".ncm-converted-")
            .suffix(&format!(".{target_format}"))
            .tempfile_in(output_dir)
            .context("无法创建转码临时文件")?;
        run_ffmpeg(
            decoded.path(),
            converted.path(),
            args.format,
            args.codec,
            ncm.metadata(),
        )?;
        write_audio_tags(converted.path(), ncm.metadata(), ncm.cover())?;
        converted.as_file_mut().sync_all()?;
        commit(converted, &output_path, args.force)?;
    } else {
        write_audio_tags(decoded.path(), ncm.metadata(), ncm.cover())?;
        decoded.as_file_mut().sync_all()?;
        commit(decoded, &output_path, args.force)?;
    }
    if args.lyrics {
        let lyrics_path = output_path.with_extension("lrc");
        let music_id = ncm.metadata().music_id();
        match save_for_track(music_id.as_deref(), source, &output_path) {
            Ok(LyricsResult::Local) => {
                eprintln!(
                    "{}: 使用本地歌词 -> {}",
                    source.display(),
                    lyrics_path.display()
                );
            }
            Ok(LyricsResult::Online) => {
                eprintln!(
                    "{}: 已从网易云保存歌词 -> {}",
                    source.display(),
                    lyrics_path.display()
                );
            }
            Ok(LyricsResult::MissingId) => {
                eprintln!(
                    "{}: 未找到本地 LRC，NCM 缺少歌曲 ID，无法联网查询歌词",
                    source.display()
                );
            }
            Ok(LyricsResult::NoLyrics) => {
                eprintln!("{}: 未找到本地 LRC，网易云未返回歌词", source.display());
            }
            Ok(LyricsResult::OutputExists) => {
                eprintln!("{}: 同名歌词文件已存在，未覆盖", source.display());
            }
            Err(error) => {
                eprintln!(
                    "{}: 音频已转换，但歌词处理失败：{error:#}",
                    source.display()
                );
            }
        }
    }
    Ok(output_path)
}

fn run_ffmpeg(
    input: &Path,
    output: &Path,
    format: OutputFormat,
    codec: AudioCodec,
    metadata: &TrackMetadata,
) -> Result<()> {
    let encoding = ffmpeg_encoding(format, codec)?;
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-map_metadata",
            "-1",
            "-c:a",
            encoding.encoder,
        ])
        .args(encoding.quality_args);
    if let Some(title) = non_empty(metadata.music_name.as_deref()) {
        command.arg("-metadata").arg(format!("title={title}"));
    }
    if let Some(album) = non_empty(metadata.album.as_deref()) {
        command.arg("-metadata").arg(format!("album={album}"));
    }
    let artists = metadata
        .artist
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|artist| artist.first().and_then(|name| name.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    if !artists.is_empty() {
        command.arg("-metadata").arg(format!("artist={artists}"));
    }
    let result = command
        .args(["-f", encoding.muxer])
        .arg(output)
        .stdin(Stdio::null())
        .output()
        .context("跨格式转换需要 ffmpeg，请先安装并加入 PATH")?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr);
        bail!("ffmpeg 转码失败：{}", detail.trim());
    }
    Ok(())
}

fn write_audio_tags(path: &Path, metadata: &TrackMetadata, cover: &[u8]) -> Result<()> {
    let artists = metadata
        .artist
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|artist| artist.first().and_then(|name| name.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let picture_mime = if cover.starts_with(b"\xff\xd8\xff") {
        Some(MimeType::Jpeg)
    } else if cover.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(MimeType::Png)
    } else {
        None
    };
    if non_empty(metadata.music_name.as_deref()).is_none()
        && non_empty(metadata.album.as_deref()).is_none()
        && artists.is_empty()
        && metadata.track.is_none()
        && picture_mime.is_none()
    {
        return Ok(());
    }

    let mut tagged_file = Probe::open(path)
        .with_context(|| format!("无法识别音频文件 {}", path.display()))?
        .read()
        .with_context(|| format!("无法读取音频标签 {}", path.display()))?;
    let tag_type = tagged_file.primary_tag_type();
    if tagged_file.tag(tag_type).is_none() {
        tagged_file.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged_file.tag_mut(tag_type).context("无法创建音频标签")?;
    if let Some(title) = non_empty(metadata.music_name.as_deref()) {
        tag.set_title(title.to_owned());
    }
    if let Some(album) = non_empty(metadata.album.as_deref()) {
        tag.set_album(album.to_owned());
    }
    if !artists.is_empty() {
        tag.set_artist(artists);
    }
    if let Some(track) = metadata.track {
        tag.insert_text(ItemKey::TrackNumber, track.to_string());
    }
    if let Some(mime) = picture_mime {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(
            Picture::unchecked(cover.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(mime)
                .build(),
        );
    }
    tagged_file
        .save_to_path(path, WriteOptions::default())
        .with_context(|| format!("无法写入音频标签 {}", path.display()))
}

fn commit(temp: NamedTempFile, destination: &Path, force: bool) -> Result<()> {
    if force {
        temp.persist(destination)
            .map(|_| ())
            .with_context(|| format!("无法写入输出文件 {}", destination.display()))
    } else {
        temp.persist_noclobber(destination)
            .map(|_| ())
            .with_context(|| format!("输出文件已存在或无法写入 {}", destination.display()))
    }
}

fn output_stem(metadata: &TrackMetadata, source: &Path) -> String {
    let title = non_empty(metadata.music_name.as_deref())
        .map(str::to_owned)
        .or_else(|| {
            source
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "track".to_owned());
    let artists = metadata
        .artist
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|artist| artist.first().and_then(|name| name.as_str()))
        .filter(|artist| !artist.trim().is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let stem = if artists.is_empty() {
        title
    } else {
        format!("{artists} - {title}")
    };
    sanitize_filename(&stem)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn sanitize_filename(name: &str) -> String {
    let replaced = name
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let trimmed = replaced.trim().trim_end_matches(&[' ', '.'][..]);
    if trimmed.is_empty() || matches!(trimmed, "." | "..") {
        "track".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_safe_and_keeps_readable_characters() {
        assert_eq!(sanitize_filename("歌手 / 歌名: 现场."), "歌手 _ 歌名_ 现场");
        assert_eq!(sanitize_filename("..."), "track");
    }

    #[test]
    fn defaults_to_original_and_keeps_lyrics_opt_in() {
        let args = Args::try_parse_from(["ncm-audio"]).unwrap();
        assert_eq!(args.format, OutputFormat::Original);
        assert_eq!(args.codec, AudioCodec::Alac);
        assert!(!args.lyrics);
    }

    #[test]
    fn m4a_and_mp4_select_the_requested_codec() {
        let codecs = [
            (AudioCodec::Alac, "alac", &[][..]),
            (AudioCodec::Aac, "aac", &["-b:a", "192k"][..]),
            (AudioCodec::Mp3, "libmp3lame", &["-q:a", "2"][..]),
        ];
        for format in [OutputFormat::M4a, OutputFormat::Mp4] {
            for (codec, encoder, quality_args) in codecs {
                let encoding = ffmpeg_encoding(format, codec).unwrap();
                assert_eq!(encoding.muxer, "mp4");
                assert_eq!(encoding.encoder, encoder);
                assert_eq!(encoding.quality_args, quality_args);
            }
        }
    }

    #[test]
    fn container_extensions_and_transcoding_are_preserved() {
        assert_eq!(OutputFormat::M4a.extension("flac"), "m4a");
        assert_eq!(OutputFormat::Mp4.extension("flac"), "mp4");
        assert!(OutputFormat::M4a.needs_transcode("m4a"));
        assert!(OutputFormat::Mp4.needs_transcode("mp4"));
        assert!(!OutputFormat::Original.needs_transcode("flac"));
        assert!(!OutputFormat::Mp3.needs_transcode("mp3"));
    }

    #[test]
    fn existing_flac_and_mp3_encoders_are_unchanged() {
        let flac = ffmpeg_encoding(OutputFormat::Flac, AudioCodec::Alac).unwrap();
        assert_eq!(flac.encoder, "flac");
        assert_eq!(flac.muxer, "flac");
        let mp3 = ffmpeg_encoding(OutputFormat::Mp3, AudioCodec::Alac).unwrap();
        assert_eq!(mp3.encoder, "libmp3lame");
        assert_eq!(mp3.muxer, "mp3");
    }
}
