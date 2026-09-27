# AGENTS.md

## 项目概览

这是一个 Rust 命令行工具，用于解密网易云音乐 `.ncm` 音频文件，可保留原始音频格式或转码为 FLAC、M4A、MP4、MP3。它还能把 NCM 元数据和封面写入输出音频，并可选择复制同名本地 LRC 歌词或联网查询歌词。

项目包含两个 Rust 2021 package：根目录的 `ncm-audio` 可执行程序，以及通过本地路径依赖使用的 `lyrics-fetcher` 库。根目录没有显式声明 Cargo workspace；两个 package 各有自己的 `Cargo.toml` 和 `Cargo.lock`。

## 目录与模块

- `Cargo.toml`、`Cargo.lock`：主程序 package 的依赖清单和锁定版本。主依赖包括 `aes`、`anyhow`、`base64`、`clap`、`serde`、`serde_json`、`lofty`、`tempfile`，以及本地 `lyrics-fetcher`。
- `src/main.rs`：CLI 定义及文件处理流程。解析输入和选项、收集 `.ncm` 文件、调用解密模块、按需运行 FFmpeg、写入标签、提交输出文件，并可调用歌词库。相关单元测试也放在此文件的 `#[cfg(test)]` 模块中。
- `src/ncm.rs`：NCM 文件解析和解密。读取文件头、密钥、元数据、封面和音频区，校验结构及大小限制，解密并暴露音频格式、曲目信息、封面和流式写出解密音频的方法。测试使用合成 NCM fixture。
- `lyrics-fetcher/Cargo.toml`、`lyrics-fetcher/Cargo.lock`：歌词库自己的 package 清单与锁文件。依赖 `anyhow`、`serde_json`、`tempfile`、`ureq`。
- `lyrics-fetcher/src/lib.rs`：查找同名本地 `.lrc`、解析并规范化网易云歌词响应、保存歌词；测试覆盖歌词解析和本地文件优先及不覆盖已有输出等行为。

仓库中未发现 README、专用格式化或 lint 配置、CI workflow、打包清单或部署脚本。不要假定存在未配置的发布流程；目前从 Cargo manifest 和源代码可确认的构建方式是使用 Cargo 编译并运行生成的程序。

## 技术栈与运行流程

- Rust 2021；CLI 使用 `clap` derive，NCM 元数据使用 `serde` / `serde_json` 解析，错误通过 `anyhow` 及上下文信息返回。
- 主程序默认将当前目录作为输入；也可传入文件或目录。目录默认只扫描当前层，使用 `--recursive` / `-r` 才递归。收集器不会跟随符号链接。
- 找到的输入按路径排序、去重后逐个转换。没有输入文件时报告错误；单个文件失败会记录错误并继续处理其余文件，最后以整体失败结果退出。
- 默认输出格式为 `original`，默认输出目录为 `output`。目标文件名优先由艺术家和歌曲名生成，并清理文件系统不安全字符；缺少歌曲名时使用输入文件名。默认不覆盖已有音频，`--force` 才允许覆盖。
- 解密音频和转码结果先写入输出目录中的临时文件，再提交为目标文件。转换后的音频可写入标题、专辑、艺术家、音轨号及 JPEG/PNG 封面。
- 选择 `flac`、`mp3` 或 `m4a` / `mp4` 且需要跨格式转换时，程序通过 PATH 调用本机 `ffmpeg`。M4A/MP4 默认采用 ALAC，可用 `--codec aac`（192 kbit/s）或 `--codec mp3`；FLAC 和 MP3 使用各自的 FFmpeg 编码器。`original` 不转码；输出扩展名与原格式相同时也不需要转码。仅在需要转码时依赖 FFmpeg。
- 歌词功能默认关闭，使用 `--lyrics` 开启。程序先在源文件旁查找同名 `.lrc`，再查找输出音频旁的同名歌词；找不到本地歌词时，只有存在有效歌曲 ID 才会请求网易云接口。歌词输出扩展名为 `.lrc`，已有同名歌词不会被覆盖。歌词请求设置 8 秒超时并限制响应和歌词大小。

常用选项：`--format` / `-f`（`original`、`flac`、`m4a`、`mp4`、`mp3`）、`--codec`、`--output` / `-o`、`--recursive` / `-r`、`--lyrics`、`--force`。可运行 `cargo run -- --help` 查看当前 CLI 帮助。

## 构建与测试

在项目根目录执行：

```sh
cargo build
cargo build --release
cargo test
cargo run -- --help
```

主程序测试通过 `src/main.rs` 的内嵌单元测试验证格式选择、转码编码参数、文件名清理和默认参数。NCM 解密测试位于 `src/ncm.rs`，覆盖合成文件解密、元数据、歌曲 ID 和无效文件头。

歌词库是单独的 package；为了运行它自己的测试，执行：

```sh
cargo test --manifest-path lyrics-fetcher/Cargo.toml
```

歌词库测试位于 `lyrics-fetcher/src/lib.rs`，主要验证响应解析、歌词规范化、无效 ID、本地歌词优先以及避免覆盖已有歌词。测试不需要真实歌词 API 响应；联网查询只发生在实际运行程序并启用 `--lyrics`、且未找到本地歌词时。

项目未提供自定义 rustfmt 配置；遵循 Rust 默认格式化风格，可用 `cargo fmt --check` 检查根 package，并用 `cargo fmt --manifest-path lyrics-fetcher/Cargo.toml -- --check` 检查歌词 package。

## 开发约定与安全边界

- 保持 Rust 2021 代码风格和现有命名、模块组织；CLI 提示和错误信息以中文为主。
- 依照现有模式使用 `anyhow::Result`、`Context`、`ensure!` 传递错误和补充上下文；新测试优先放在被测模块附近的 `#[cfg(test)]` 中。
- 处理外部 NCM 文件时保留现有结构和大小校验；新增解析路径时不要绕过密钥、元数据、封面和格式校验。
- 输入收集明确拒绝符号链接；输出使用临时文件，并默认不覆盖已有音频。歌词保存也使用临时文件，并始终避免覆盖已有 `.lrc`。
- 联网歌词请求只应由显式 `--lyrics` 选项触发；保留歌曲 ID 校验、请求超时和响应大小上限。
- 转码依赖运行环境的 `ffmpeg` 可执行文件，不由 Cargo 安装或随程序打包。未进行转码时无需它。
