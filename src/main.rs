mod ncm;

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{Accessor, ItemKey};
use lofty::probe::Probe;
use lofty::tag::Tag;
use lyrics_fetcher::{save_for_track, LocalLyricsChoice, LyricsResult};
use ncm::{NcmAudio, TrackMetadata};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    override_usage = "ncm-audio [-r] [-l] [-f] [格式] [输入文件或目录] [输出目录或文件]",
    after_help = "格式：original（默认）、flac、mp3、m4a、m4a-aac、m4a-mp3、mp4、mp4-aac、mp4-mp3。M4A/MP4 默认使用无损 ALAC。输出文件须带与实际音频格式一致的扩展名；不带扩展名的输出路径视为目录，默认为 output。转码需要本机 ffmpeg。"
)]
struct Args {
    #[arg(value_name = "ARG", num_args = 0..=3, help = "[格式] [输入路径] [输出目录或文件]")]
    positionals: Vec<PathBuf>,

    #[arg(short, long, help = "递归扫描输入目录")]
    recursive: bool,

    #[arg(short, long, help = "源文件有同名 .lrc 时询问选择，否则联网查询")]
    lyrics: bool,

    #[arg(short, long, help = "覆盖同名输出文件")]
    force: bool,
}

struct RunArgs {
    input: PathBuf,
    output: PathBuf,
    format: OutputFormat,
    codec: AudioCodec,
    recursive: bool,
    lyrics: bool,
    force: bool,
}

fn parse_format(value: &str) -> Option<(OutputFormat, AudioCodec)> {
    let (format, codec) = match value {
        "original" => (OutputFormat::Original, AudioCodec::Alac),
        "flac" => (OutputFormat::Flac, AudioCodec::Alac),
        "mp3" => (OutputFormat::Mp3, AudioCodec::Alac),
        "m4a" | "m4a-alac" => (OutputFormat::M4a, AudioCodec::Alac),
        "m4a-aac" => (OutputFormat::M4a, AudioCodec::Aac),
        "m4a-mp3" => (OutputFormat::M4a, AudioCodec::Mp3),
        "mp4" | "mp4-alac" => (OutputFormat::Mp4, AudioCodec::Alac),
        "mp4-aac" => (OutputFormat::Mp4, AudioCodec::Aac),
        "mp4-mp3" => (OutputFormat::Mp4, AudioCodec::Mp3),
        _ => return None,
    };
    Some((format, codec))
}

impl Args {
    fn resolve(self) -> Result<RunArgs> {
        let mut values = self.positionals.into_iter();
        let first = values.next();
        let selected = first
            .as_ref()
            .and_then(|value| value.to_str())
            .and_then(parse_format);
        let (format, codec) = selected.unwrap_or((OutputFormat::Original, AudioCodec::Alac));
        let input = if selected.is_some() {
            values.next()
        } else {
            first
        }
        .unwrap_or(std::env::current_dir()?);
        let output = values.next().unwrap_or_else(|| PathBuf::from("output"));
        ensure!(
            values.next().is_none(),
            "参数过多；用法：[格式] [输入路径] [输出路径]"
        );
        Ok(RunArgs {
            input,
            output,
            format,
            codec,
            recursive: self.recursive,
            lyrics: self.lyrics,
            force: self.force,
        })
    }
}

fn main() -> Result<()> {
    let args = Args::parse().resolve()?;
    let mut files = Vec::new();
    collect_ncm_files(&args.input, args.recursive, &mut files)?;
    ensure!(!files.is_empty(), "没有找到 .ncm 文件");
    if args.input.is_dir() {
        ensure!(
            !output_is_file(&args.output),
            "输入目录不能指定单个输出文件：{}",
            args.output.display()
        );
    }

    let mut failures = 0usize;
    for path in &files {
        match convert(path, &args) {
            Ok(ConversionOutcome::Written(output)) => {
                println!("{} -> {}", path.display(), output.display())
            }
            Ok(ConversionOutcome::Kept(output)) => {
                println!("{}: 保留已有音频 {}", path.display(), output.display())
            }
            Ok(ConversionOutcome::Skipped) => {
                println!("{}: 已跳过", path.display())
            }
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

fn output_is_file(path: &Path) -> bool {
    !path.is_dir()
        && !path.as_os_str().to_string_lossy().ends_with('/')
        && path.extension().is_some()
}

fn output_path(
    target: &Path,
    metadata: &TrackMetadata,
    source: &Path,
    format: &str,
) -> Result<PathBuf> {
    if output_is_file(target) {
        ensure!(
            target
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case(format)),
            "输出文件扩展名必须是 .{format}：{}",
            target.display()
        );
        Ok(target.to_path_buf())
    } else {
        Ok(target.join(format!("{}.{}", output_stem(metadata, source), format)))
    }
}

fn read_choice(prompt: &str) -> Result<KeyCode> {
    ensure!(
        io::stdin().is_terminal(),
        "需要选择，但标准输入不是终端；请在交互终端运行"
    );
    eprint!("{prompt} ");
    io::stderr().flush()?;
    enable_raw_mode().context("无法启用终端按键读取")?;
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }
    let key = {
        let _raw = RawMode;
        loop {
            if let Event::Key(key) = event::read().context("读取选择失败")? {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'n' | 'N') | KeyCode::Esc => {
                        break key.code
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        bail!("已取消选择")
                    }
                    _ => {}
                }
            }
        }
    };
    eprintln!();
    Ok(key)
}

fn choose_local_lyrics(local: &Path) -> Result<LocalLyricsChoice> {
    let prompt = format!(
        "发现本地歌词 {}：回车/y 使用本地，n 联网，Esc 跳过歌词",
        local.display()
    );
    Ok(match read_choice(&prompt)? {
        KeyCode::Enter | KeyCode::Char('y' | 'Y') => LocalLyricsChoice::Local,
        KeyCode::Char('n' | 'N') => LocalLyricsChoice::Online,
        KeyCode::Esc => LocalLyricsChoice::Skip,
        _ => unreachable!(),
    })
}

#[derive(Clone, Copy)]
enum AudioChoice {
    Overwrite,
    Keep,
    Skip,
}

fn choose_existing_audio(path: &Path, lyrics: bool) -> Result<AudioChoice> {
    let prompt = if lyrics {
        format!(
            "检测到已有音频 {}：回车/y 覆盖，n 保留音频并获取歌词，Esc 跳过当前项",
            path.display()
        )
    } else {
        format!(
            "检测到已有音频 {}：回车/y 覆盖，n/Esc 保留并跳过当前项",
            path.display()
        )
    };
    Ok(match read_choice(&prompt)? {
        KeyCode::Enter | KeyCode::Char('y' | 'Y') => AudioChoice::Overwrite,
        KeyCode::Char('n' | 'N') if lyrics => AudioChoice::Keep,
        KeyCode::Char('n' | 'N') | KeyCode::Esc => AudioChoice::Skip,
        _ => unreachable!(),
    })
}

enum ConversionOutcome {
    Written(PathBuf),
    Kept(PathBuf),
    Skipped,
}

fn convert(source: &Path, args: &RunArgs) -> Result<ConversionOutcome> {
    let mut ncm = NcmAudio::open(source)?;
    let original_format = ncm.format().to_owned();
    let target_format = args.format.extension(&original_format).to_owned();
    let output_path = output_path(&args.output, ncm.metadata(), source, &target_format)?;
    let exists = output_path.exists();
    let choice = if !exists || args.force {
        AudioChoice::Overwrite
    } else {
        choose_existing_audio(&output_path, args.lyrics)?
    };
    if matches!(choice, AudioChoice::Skip) {
        return Ok(ConversionOutcome::Skipped);
    }
    if matches!(choice, AudioChoice::Overwrite) {
        let transcode = args.format.needs_transcode(&original_format);
        if transcode {
            let status = Command::new("ffmpeg")
                .arg("-version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("跨格式转换需要 ffmpeg，请先安装并加入 PATH")?;
            ensure!(status.success(), "ffmpeg 版本检查失败");
        }

        let output_dir = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(output_dir)
            .with_context(|| format!("无法创建输出目录 {}", output_dir.display()))?;

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
            commit(converted, &output_path, exists)?;
        } else {
            write_audio_tags(decoded.path(), ncm.metadata(), ncm.cover())?;
            decoded.as_file_mut().sync_all()?;
            commit(decoded, &output_path, exists)?;
        }
    }
    if args.lyrics {
        let lyrics_path = output_path.with_extension("lrc");
        let music_id = ncm.metadata().music_id();
        match save_for_track(
            music_id.as_deref(),
            source,
            &output_path,
            choose_local_lyrics,
        ) {
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
            Ok(LyricsResult::Skipped) => {
                eprintln!("{}: 已跳过歌词", source.display());
            }
            Ok(LyricsResult::MissingId) => {
                eprintln!(
                    "{}: 未使用本地 LRC，NCM 缺少歌曲 ID，无法联网查询歌词",
                    source.display()
                );
            }
            Ok(LyricsResult::NoLyrics) => {
                eprintln!("{}: 未使用本地 LRC，网易云未返回歌词", source.display());
            }
            Ok(LyricsResult::OutputExists) => {
                eprintln!("{}: 同名歌词文件已存在，未覆盖", source.display());
            }
            Err(error) => {
                eprintln!(
                    "{}: 音频已处理，但歌词处理失败：{error:#}",
                    source.display()
                );
            }
        }
    }
    Ok(match choice {
        AudioChoice::Overwrite => ConversionOutcome::Written(output_path),
        AudioChoice::Keep => ConversionOutcome::Kept(output_path),
        AudioChoice::Skip => unreachable!(),
    })
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
    fn missing_title_falls_back_to_input_stem_and_keeps_artist() {
        let metadata = TrackMetadata {
            artist: Some(vec![vec![serde_json::Value::String("artist".to_owned())]]),
            ..TrackMetadata::default()
        };
        assert_eq!(
            output_stem(&metadata, Path::new("input.ncm")),
            "artist - input"
        );
    }

    #[test]
    fn defaults_to_original_and_keeps_lyrics_opt_in() {
        let args = Args::try_parse_from(["ncm-audio"])
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(args.input, std::env::current_dir().unwrap());
        assert_eq!(args.output, PathBuf::from("output"));
        assert_eq!(args.format, OutputFormat::Original);
        assert_eq!(args.codec, AudioCodec::Alac);
        assert!(!args.lyrics);
    }

    #[test]
    fn accepts_format_input_and_output_positionals() {
        let args = Args::try_parse_from([
            "ncm-audio",
            "-r",
            "m4a-aac",
            "./music/song.ncm",
            "./converted/song.m4a",
        ])
        .unwrap()
        .resolve()
        .unwrap();
        assert_eq!(args.input, PathBuf::from("./music/song.ncm"));
        assert_eq!(args.output, PathBuf::from("./converted/song.m4a"));
        assert_eq!(args.format, OutputFormat::M4a);
        assert_eq!(args.codec, AudioCodec::Aac);
        assert!(args.recursive);
    }

    #[test]
    fn accepts_short_and_long_option_names() {
        for flags in [["-r", "-l", "-f"], ["--recursive", "--lyrics", "--force"]] {
            let args = Args::try_parse_from([
                "ncm-audio",
                flags[0],
                flags[1],
                flags[2],
                "song.ncm",
                "output",
            ])
            .unwrap()
            .resolve()
            .unwrap();
            assert!(args.recursive);
            assert!(args.lyrics);
            assert!(args.force);
            assert_eq!(args.input, PathBuf::from("song.ncm"));
            assert_eq!(args.output, PathBuf::from("output"));
        }
        assert!(Args::try_parse_from(["ncm-audio", "-lyrics"]).is_err());
    }

    #[test]
    fn input_and_output_without_format_keep_original() {
        let args = Args::try_parse_from(["ncm-audio", "歌曲.ncm", "converted"])
            .unwrap()
            .resolve()
            .unwrap();
        assert_eq!(args.input, PathBuf::from("歌曲.ncm"));
        assert_eq!(args.output, PathBuf::from("converted"));
        assert_eq!(args.format, OutputFormat::Original);
        assert_eq!(
            parse_format("mp4-mp3"),
            Some((OutputFormat::Mp4, AudioCodec::Mp3))
        );
        assert_eq!(
            parse_format("m4a"),
            Some((OutputFormat::M4a, AudioCodec::Alac))
        );
    }

    #[test]
    fn output_target_accepts_directory_or_matching_filename() {
        let metadata = TrackMetadata::default();
        let source = Path::new("song.ncm");
        assert_eq!(
            output_path(Path::new("converted"), &metadata, source, "mp3").unwrap(),
            PathBuf::from("converted/song.mp3")
        );
        assert_eq!(
            output_path(Path::new("renamed.mp3"), &metadata, source, "mp3").unwrap(),
            PathBuf::from("renamed.mp3")
        );
        assert!(output_path(Path::new("renamed.flac"), &metadata, source, "mp3").is_err());
        assert!(!output_is_file(Path::new("converted.v1/")));
        let directory = tempfile::tempdir().unwrap();
        let dotted_directory = directory.path().join("converted.v1");
        fs::create_dir(&dotted_directory).unwrap();
        assert!(!output_is_file(&dotted_directory));
    }

    #[test]
    fn rejects_three_paths_without_a_format() {
        let args = Args::try_parse_from(["ncm-audio", "one.ncm", "two.ncm", "output"]).unwrap();
        assert!(args.resolve().is_err());
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
