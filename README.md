# AI Jarvis

AI Jarvis 是一个仅面向 Windows 内部测试的本地多模态桌宠 MVP。当前实现包含文字对话、屏幕观察、系统声音观察、按键式麦克风输入、隐私暂停，以及 game 场景的透明弹幕覆盖层。模型推理和转写均在本机完成。

项目范围与实现状态分别以以下文档为准：

- [`docs/AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md`](docs/AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md)
- [`docs/AI_JARVIS_CORE_DEVELOPMENT_PLAN.md`](docs/AI_JARVIS_CORE_DEVELOPMENT_PLAN.md)
- [`docs/AI_JARVIS_DEVELOPMENT_CHECKLIST.md`](docs/AI_JARVIS_DEVELOPMENT_CHECKLIST.md)

## 前置依赖

- Windows 10/11 x64 与可用的 WebView2 Runtime。
- Node.js 20 或更高版本、pnpm 9 或更高版本。
- Rust 1.77.2 或更高版本，以及 MSVC x64 C++ 构建工具。
- 固定为 build `10752`、commit `b96806d96061049a5b574269b049bf6241d63d46` 的 `llama.cpp` Windows x64 可执行文件。
- `resources/config/model.toml` 固定 revision、大小和 SHA-256 的 Qwen GGUF、mmproj、SenseVoice ONNX 与 tokens 文件。
- 使用声音能力时，需要可用的默认播放/录音端点和相应的 Windows 桌面权限。

模型默认安装在 `%LOCALAPPDATA%\AIJarvis\models`。可通过 `AI_JARVIS_MODEL_DIR` 或脚本的 `-ModelRoot` 参数使用其他绝对目录。模型、录音 fixture 和生成的 ASR sidecar 不提交到 Git。

## 环境检查

```powershell
pnpm install --frozen-lockfile
pwsh -NoProfile -File scripts/setup.ps1
```

`setup.ps1` 会一次列出工具链、WebView2、Windows 采集原语、llama.cpp 和所有模型文件的状态。退出码为 `0` 表示依赖完整，`1` 表示存在可修复的缺失项，`2` 表示探测器本身失败。

模型文件可按固定配置下载：

```powershell
pwsh -NoProfile -File scripts/download-models.ps1
```

下载器优先使用 `hf-mirror.com`，明确失败后才切换到同一 revision 的 `huggingface.co`，并强制校验文件大小和 SHA-256。它不安装 llama.cpp；llama.cpp 路径必须能由 `setup.ps1` 解析或通过 `-LlamaExecutable` 显式传入。

## 本地运行

```powershell
pnpm tauri dev
```

Tauri 启动前会构建 `asr_worker` sidecar 并准备 sherpa-onnx 所需 DLL。模型缺失时桌宠与隐私控制仍可启动，但 Qwen 回复和 SenseVoice 转写会明确不可用，不会使用 fallback 模型或硬编码 prompt。

仅检查 llama.cpp 的本地非思考启动参数：

```powershell
pwsh -NoProfile -File scripts/run-llama.ps1 -DryRun -NoUi
```

## 构建

```powershell
pnpm tauri build --debug --no-bundle
```

内部 debug 可执行文件生成在 `src-tauri/target/debug/ai-jarvis.exe`。当前项目未配置签名、安装器、自动更新或公开分发流程。

## 验证

常规本地门禁：

```powershell
pnpm build
pnpm test
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

以下真实契约默认 ignored，只有在固定模型和许可明确的 WAV fixture 已安装时才执行：

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test model_contract -- --ignored
cargo test --manifest-path src-tauri/Cargo.toml --test asr_contract -- --ignored
```

ASR fixture 的格式和隐私要求记录在 `src-tauri/tests/fixtures/audio/README.md`。不得用 fake transcript 代替真实模型门禁。

## 隐私边界

- 隐私暂停会停止屏幕、系统声音和麦克风采集，取消在途模型任务，并清空气泡与弹幕等瞬时输出。
- 系统声音转写只能进入不可信 observation 字段，不能成为 user role 或 direct chat。
- 屏幕帧、系统音频、转写和聊天历史不做长期持久化；麦克风只在用户主动开始后创建有界临时 PCM 文件，提交、取消、暂停、退出和启动清理都会删除它。
- WebView capability 不包含 shell、文件系统或 HTTP 权限。

## 已知限制

- 当前只针对桌宠所在的单一显示器；未做多显示器专门适配。
- overlay 支持普通窗口化和无边框场景所需的透明、置顶、点击穿透与不抢焦点属性，但不保证独占全屏或带反作弊保护的游戏兼容。
- 仍需在测试者选定的一款真实窗口化或无边框游戏上完成 overlay 实测。
- Qwen GGUF/mmproj、SenseVoice ONNX/tokens 和许可明确的音频 fixture 缺失时，真实模型契约保持未通过。
- MVP 不包含课程、长期记忆、SQLite、托盘、TTS、签名、安装器、自动更新或 CI。
- 快速 smoke 退出时 WebView2 偶尔记录 `Chrome_WidgetWin_0` unregister `Error=1412`；当前退出码与进程清理正常，尚未观察到用户可见影响。
