use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::Builder;

const MAX_RESPONSE_SIZE: u64 = 512 * 1024;
const MAX_LYRIC_SIZE: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum LyricsResult {
    Local,
    Online,
    MissingId,
    NoLyrics,
    OutputExists,
}

pub fn save_for_track(
    music_id: Option<&str>,
    source: &Path,
    output: &Path,
) -> Result<LyricsResult> {
    let destination = output.with_extension("lrc");
    if let Some(local) = find_local_lrc(source, output) {
        if let Ok(contents) = fs::read(&local) {
            if !contents.is_empty() {
                return Ok(if write_new(&destination, &contents)? {
                    LyricsResult::Local
                } else {
                    LyricsResult::OutputExists
                });
            }
        }
    }

    let Some(music_id) = music_id else {
        return Ok(LyricsResult::MissingId);
    };
    let Some(contents) = fetch_lyric(music_id)? else {
        return Ok(LyricsResult::NoLyrics);
    };
    Ok(if write_new(&destination, contents.as_bytes())? {
        LyricsResult::Online
    } else {
        LyricsResult::OutputExists
    })
}

fn find_local_lrc(source: &Path, output: &Path) -> Option<PathBuf> {
    matching_lrc(source).or_else(|| matching_lrc(output))
}

fn matching_lrc(audio: &Path) -> Option<PathBuf> {
    let stem = audio.file_stem()?.to_string_lossy();
    let parent = audio
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut matches = fs::read_dir(parent)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_file() {
                return None;
            }
            let path = entry.path();
            let extension = path.extension()?.to_str()?;
            let candidate_stem = path.file_stem()?.to_string_lossy();
            (extension.eq_ignore_ascii_case("lrc")
                && candidate_stem.eq_ignore_ascii_case(stem.as_ref()))
            .then_some(path)
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.into_iter().next()
}

fn fetch_lyric(music_id: &str) -> Result<Option<String>> {
    ensure!(
        !music_id.is_empty()
            && music_id.len() <= 20
            && music_id.bytes().all(|byte| byte.is_ascii_digit()),
        "网易云歌曲 ID 无效"
    );
    let url = format!(
        "https://music.163.com/api/song/lyric/v1?tv=0&lv=0&rv=0&kv=0&yv=0&ytv=0&yrv=0&cp=false&id={music_id}"
    );
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(8))
        .set("User-Agent", "lyrics-fetcher/0.1")
        .call()
        .context("网易云歌词请求失败")?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_SIZE + 1)
        .read_to_end(&mut body)
        .context("读取网易云歌词响应失败")?;
    ensure!(
        body.len() as u64 <= MAX_RESPONSE_SIZE,
        "网易云歌词响应超出大小限制"
    );
    parse_lyric_response(&body)
}

fn parse_lyric_response(body: &[u8]) -> Result<Option<String>> {
    let response: Value = serde_json::from_slice(body).context("网易云歌词响应 JSON 无效")?;
    let code = response
        .get("code")
        .and_then(Value::as_i64)
        .or_else(|| response.get("code").and_then(Value::as_str)?.parse().ok());
    if let Some(code) = code.filter(|code| *code != 200) {
        bail!("网易云歌词接口返回状态码 {code}");
    }
    let Some(lyric) = response
        .pointer("/lrc/lyric")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|lyric| !lyric.is_empty())
    else {
        return Ok(None);
    };
    let Some(lyric) = normalize_lyric(lyric) else {
        return Ok(None);
    };
    ensure!(lyric.len() <= MAX_LYRIC_SIZE, "歌词内容超出大小限制");
    Ok(Some(lyric))
}

fn normalize_lyric(lyric: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut has_lyrics = false;
    for raw in lyric.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('{') {
            if let Ok(credit) = serde_json::from_str::<Value>(line) {
                let timestamp = credit.get("t").and_then(Value::as_u64);
                let text = credit
                    .get("c")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("tx").and_then(Value::as_str))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                if let Some(timestamp) = timestamp {
                    if !text.trim().is_empty() {
                        lines.push(format!("{}{}", format_timestamp(timestamp), text.trim()));
                    }
                }
            }
            continue;
        }
        let text = line.split_once(']').map_or(line, |(_, text)| text).trim();
        if !text.is_empty() && !is_metadata_tag(line) {
            has_lyrics = true;
        }
        lines.push(line.to_owned());
    }
    has_lyrics.then(|| format!("{}\n", lines.join("\n")))
}

fn format_timestamp(milliseconds: u64) -> String {
    let centiseconds = milliseconds.saturating_add(5) / 10;
    format!(
        "[{:02}:{:02}.{:02}]",
        centiseconds / 6000,
        centiseconds / 100 % 60,
        centiseconds % 100
    )
}

fn is_metadata_tag(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|line| line.split_once(':'))
        .is_some_and(|(tag, _)| {
            !tag.is_empty() && tag.chars().all(|character| character.is_ascii_alphabetic())
        })
}

fn write_new(path: &Path, contents: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = Builder::new()
        .prefix(".lyrics-fetcher-")
        .tempfile_in(parent)
        .with_context(|| format!("无法在 {} 创建歌词临时文件", parent.display()))?;
    temp.write_all(contents)?;
    temp.as_file_mut().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            Err(error.error).with_context(|| format!("无法保存歌词文件 {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_timed_lyric_response() -> Result<()> {
        let body =
            r#"{"code":200,"lrc":{"version":1,"lyric":"[00:01.00]第一句\n[00:02.00]第二句"}}"#
                .as_bytes();
        let lyric = parse_lyric_response(body)?.unwrap();
        assert_eq!(lyric, "[00:01.00]第一句\n[00:02.00]第二句\n");
        Ok(())
    }

    #[test]
    fn converts_credit_json_to_timed_lrc_lines() -> Result<()> {
        let body = r#"{"code":200,"lrc":{"version":1,"lyric":"{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"Mili\"}]}\n[00:01.00]第一句"}}"#.as_bytes();
        let lyric = parse_lyric_response(body)?.unwrap();
        assert_eq!(lyric, "[00:00.00]作词: Mili\n[00:01.00]第一句\n");
        Ok(())
    }

    #[test]
    fn rejects_non_success_response() {
        assert!(parse_lyric_response(br#"{"code":404}"#).is_err());
    }

    #[test]
    fn missing_lyrics_returns_none() -> Result<()> {
        assert_eq!(
            parse_lyric_response(br#"{"code":200,"nolyric":true}"#)?,
            None
        );
        let credits_only = r#"{"code":200,"lrc":{"lyric":"{\"t\":0,\"c\":[{\"tx\":\"作词: \"},{\"tx\":\"Mili\"}]}"}}"#.as_bytes();
        assert_eq!(parse_lyric_response(credits_only)?, None);
        Ok(())
    }

    #[test]
    fn invalid_music_id_is_rejected_before_request() {
        assert!(fetch_lyric("not-a-number").is_err());
    }

    #[test]
    fn prefers_local_lrc_and_saves_it_beside_output() -> Result<()> {
        let source_dir = tempdir()?;
        let output_dir = tempdir()?;
        let source = source_dir.path().join("song.ncm");
        let local = source_dir.path().join("song.LRC");
        let output = output_dir.path().join("song.flac");
        let expected = b"[00:01.00]local lyrics\n";
        fs::write(&local, expected)?;

        assert_eq!(
            save_for_track(Some("invalid-id"), &source, &output)?,
            LyricsResult::Local
        );
        assert_eq!(fs::read(output.with_extension("lrc"))?, expected);
        Ok(())
    }

    #[test]
    fn existing_output_lrc_is_never_overwritten() -> Result<()> {
        let source_dir = tempdir()?;
        let output_dir = tempdir()?;
        let source = source_dir.path().join("song.ncm");
        let local = source_dir.path().join("song.lrc");
        let output = output_dir.path().join("song.flac");
        let destination = output.with_extension("lrc");
        fs::write(&local, b"local lyrics\n")?;
        fs::write(&destination, b"keep existing lyrics\n")?;

        assert_eq!(
            save_for_track(None, &source, &output)?,
            LyricsResult::OutputExists
        );
        assert_eq!(fs::read(destination)?, b"keep existing lyrics\n");
        Ok(())
    }
}
