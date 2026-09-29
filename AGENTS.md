# AGENTS.md

## 项目概览

这是一个 Rust 命令行工具，用于解密网易云音乐 `.ncm` 音频文件，可保留原始音频格式或转码为 FLAC、M4A、MP4、MP3。它还能把 NCM 元数据和封面写入输出音频，并可交互选择源文件旁的本地 LRC 歌词或联网查询歌词。

项目包含两个 Rust 2021 package：根目录的 `ncm-audio` 可执行程序，以及通过本地路径依赖使用的 `lyrics-fetcher` 库。根目录没有显式声明 Cargo workspace；两个 package 各有自己的 `Cargo.toml` 和 `Cargo.lock`。根目录的 `rust-toolchain.toml` 通过 Rustup 选择 stable 工具链并包含 `rustfmt`；首次使用需联网获取工具链，但不要求设置全局默认版本。

## 文档职责与同步

- `README.md` 面向使用者，介绍项目、构建与运行方式、CLI 参数、外部依赖及用户可见行为；避免承载代理专用流程或重复开发规范。
- `AGENTS.md` 面向贡献者和编码代理，维护代码结构、实现约定、验证命令及安全边界；不替代 README 的用户指南。
- 每项项目变更在整体任务完成前，都要检查受影响的 Markdown 文档并同步更新；只更新与本次变更相关的文档，避免无关改动。文档更新属于完成任务的一部分。

## 目录与模块

- `Cargo.toml`、`Cargo.lock`：主程序 package 的依赖清单和锁定版本。主依赖包括 `aes`、`anyhow`、`base64`、`clap`、`crossterm`、`crc32fast`、`serde`、`serde_json`、`lofty`、`tempfile`，以及本地 `lyrics-fetcher`。
- `rust-toolchain.toml`：通过 Rustup 选择 stable 工具链并安装 `rustfmt` 组件。
- `setup.sh`：在 POSIX shell 中按需安装 Rustup，运行 release 构建，通过临时文件原子替换根目录的 `ncm-audio`；不设置 Rustup 全局默认版本。
- 根目录 `ncm-audio`：运行 `setup.sh` 后生成的无后缀二进制，可移动到兼容的运行环境，不会在执行时调用 Cargo。
- `src/main.rs`：CLI 定义及文件处理流程。解析输入和选项、收集 `.ncm` 文件、调用解密模块、按需运行 FFmpeg、写入标签、提交输出文件，并可调用歌词库。相关单元测试也放在此文件的 `#[cfg(test)]` 模块中。
- `src/ncm.rs`：NCM 文件解析和解密。读取文件头、密钥、元数据、封面和音频区，校验文件前缀 CRC32、结构及大小限制，解密并暴露音频格式、曲目信息、封面和流式写出解密音频的方法。测试使用合成 NCM fixture。
- `lyrics-fetcher/Cargo.toml`、`lyrics-fetcher/Cargo.lock`：歌词库自己的 package 清单与锁文件。依赖 `anyhow`、`serde_json`、`tempfile`、`ureq`。
- `lyrics-fetcher/src/lib.rs`：查找源文件旁的同名本地 `.lrc`、解析并规范化网易云歌词响应、保存歌词；歌词命中时由调用方提供本地 / 联网 / 跳过选择，已有输出不覆盖。

仓库未提供专用格式化或 lint 配置、CI workflow、打包清单或发布流程。根目录 `setup.sh` 是供 Linux、macOS、WSL 等 POSIX shell 使用的一键 Rustup 引导与 release 构建脚本；不要假定存在未配置的发布流程。

## 技术栈与运行流程

- Rust 2021；CLI 使用 `clap` derive，NCM 元数据使用 `serde` / `serde_json` 解析，错误通过 `anyhow` 及上下文信息返回。
- CLI 位置参数依次为可选格式、可选输入文件或目录、可选输出目录或文件；省略输入时扫描当前工作目录。目录默认只扫描当前层，使用 `--recursive` / `-r` 才递归。输入与输出的相对路径按调用时的当前工作目录解释，与二进制位置无关；收集器不会跟随符号链接。
- 找到的输入逐个转换。没有输入文件时报告错误；单个文件失败会记录错误并继续处理其余文件，最后以整体失败结果退出。一次接受一个输入路径。
- 默认输出格式为 `original`，默认输出目录是当前工作目录下的 `output/`。现有目录或无扩展名的输出路径按目录处理；带扩展名的路径按完整文件名处理，要求扩展名与音频格式相同，且目录输入不得指定单个输出文件。输出到目录时目标文件名优先使用艺术家和歌曲名；缺少歌曲名时以输入文件名（不含扩展名）代替歌曲名，若艺术家存在仍会保留艺术家前缀。文件名中的不安全字符会被替换。已有同名音频时，交互选择回车 / y 覆盖；启用 `-l` 时 n 保留音频并继续歌词、Esc 跳过当前项；未启用 `-l` 时 n / Esc 均保留音频并跳过当前项。`-f` / `--force` 直接覆盖，非交互输入不自动选择。
- 解密音频和转码结果先写入输出目标所在目录的临时文件，再提交为目标文件。转换后的音频可写入标题、专辑、艺术家、音轨号及 JPEG/PNG 封面。
- 选择 `flac` 或 `mp3` 且输出扩展名与输入格式不同时，或选择 `m4a` / `mp4` 时，程序通过 PATH 调用本机 `ffmpeg`。M4A/MP4 始终转码，默认采用 ALAC，可用格式 `m4a-aac` / `mp4-aac`（192 kbit/s）或 `m4a-mp3` / `mp4-mp3`；FLAC 和 MP3 使用各自的 FFmpeg 编码器。`original` 不转码；FLAC 或 MP3 输出扩展名与输入格式相同时也不需要转码。仅在需要转码时依赖 FFmpeg。
- 歌词功能默认关闭，使用 `-l` / `--lyrics` 开启。已有输出歌词直接保留；源文件旁有可读取的非空同名 `.lrc` 时，在交互终端询问：回车 / y 用本地、n 联网、Esc 跳过；非交互标准输入不自动选择，仅报告歌词处理失败。没有本地歌词时，只有存在有效歌曲 ID 才会请求网易云接口。歌词输出扩展名为 `.lrc`，已有同名歌词不会被覆盖。歌词请求设置 8 秒超时并限制响应和歌词大小。

常用参数：位置格式 `original`、`flac`、`mp3`、`m4a` / `mp4`（含 `-alac`、`-aac`、`-mp3` 编码后缀）、`-r` / `--recursive`、`-l` / `--lyrics`、`-f` / `--force`。可运行 `./ncm-audio --help` 查看当前 CLI 帮助。

## 构建与测试

在项目根目录执行：

```sh
cargo build
cargo build --release
cargo test
cargo test --manifest-path lyrics-fetcher/Cargo.toml
cargo fmt --check
cargo fmt --manifest-path lyrics-fetcher/Cargo.toml -- --check
seiso check README.md AGENTS.md
sh ./setup.sh
./ncm-audio -h
```

直接运行 `cargo build --release` 只生成 `target/release/ncm-audio`；`setup.sh` 会把它原子替换到根目录。主程序测试位于 `src/main.rs` 和 `src/ncm.rs`，覆盖 CLI 参数、格式选择、转码编码参数、输出路径和合成 NCM 解密。

歌词库是单独的 package；为了运行它自己的测试，执行：

```sh
cargo test --manifest-path lyrics-fetcher/Cargo.toml
```

歌词库测试位于 `lyrics-fetcher/src/lib.rs`，主要验证响应解析、歌词规范化、无效 ID、本地歌词选择及避免覆盖已有歌词。测试不需要真实歌词 API 响应；联网查询只发生在实际运行程序并启用 `-l` / `--lyrics`、且未找到本地歌词或明确选择联网时。

项目未提供自定义 rustfmt 配置；遵循 Rust 默认格式化风格，可用 `cargo fmt --check` 检查根 package，并用 `cargo fmt --manifest-path lyrics-fetcher/Cargo.toml -- --check` 检查歌词 package。

## 开发约定与安全边界

- 保持 Rust 2021 代码风格和现有命名、模块组织；CLI 提示和错误信息以中文为主。
- 依照现有模式使用 `anyhow::Result`、`Context`、`ensure!` 传递错误和补充上下文；新测试优先放在被测模块附近的 `#[cfg(test)]` 中。
- 处理外部 NCM 文件时保留现有结构和大小校验；新增解析路径时不要绕过密钥、元数据、封面和格式校验。
- 输入收集明确拒绝符号链接；输出使用临时文件，并默认不覆盖已有音频。歌词保存也使用临时文件，并始终避免覆盖已有 `.lrc`。
- 联网歌词请求只应由显式 `-l` / `--lyrics` 选项触发；保留歌曲 ID 校验、请求超时和响应大小上限。
- 转码依赖运行环境的 `ffmpeg` 可执行文件，不由 Cargo 安装或随程序打包。未进行转码时无需它。
