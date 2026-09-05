# AI Jarvis 开发清单

本文件只记录可由代码、测试或真机验证证明的状态，不使用百分比。设计范围以
`AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md` 为产品目标，以
`AI_JARVIS_CORE_DEVELOPMENT_PLAN.md` 的 MVP 边界和依赖顺序为当前执行依据。

## 当前状态

- 当前工作单元：`AG-05 屏幕观察与主动响应`
- 状态：`in_progress`
- 下一依赖单元：`AG-06 系统声音与主动语音`
- 当前外部阻断：Qwen GGUF、mmproj 和 SenseVoice ONNX 尚未安装；AG-04 的代码、fake 集成和 UI 已完成，但 AG-03 真实模型契约以及 AG-04 真实回复仍无法验证；不阻断 AG-05/AG-06 的代码与 fake 开发
- 模型下载源：优先 `https://hf-mirror.com`；镜像明确失败后按用户授权切换到固定 revision 的 `https://huggingface.co`
- 最近审计基线：`d9e5eea docs: record AG-04 partial verification`

## 状态定义

- `not_started`：依赖未满足或尚无实现。
- `in_progress`：正在实现，门禁尚未全部执行。
- `partial`：已有可用实现，但至少一个门禁未通过。
- `blocked`：外部模型、驱动、硬件、权限或必要决策阻断，并有可复现证据。
- `verified`：本工作单元的全部门禁通过。

## MVP 工作单元

### AG-00 基线与外部契约

状态：`partial`

- [x] 固定 llama.cpp build/commit、可执行文件、大小和 SHA-256。
- [x] 固定 Qwen GGUF/mmproj 和 SenseVoice 文件的 revision、路径、大小和 SHA-256。
- [x] `setup.ps1` 检查 Node、pnpm、Rust、WebView2、llama.cpp 和模型文件。
- [x] 下载器先使用 `https://hf-mirror.com`，仅在失败后明确切换到同一 revision 的 `https://huggingface.co`，最终强制大小与 SHA-256 校验。
- [x] `run-llama.ps1` 使用独立 argv、本地模型路径和 `--reasoning off`，且不含 `enable_thinking`。
- [x] 分层探测 WGC、音频端点、麦克风策略和透明点击穿透窗口的 OS 原语；真实帧、音频和窗口命中 smoke 留给对应工作单元。
- [x] fixture 验证完整依赖返回 0，缺失依赖返回 1，探测器错误返回 2，并一次列出全部缺失项。
- [x] 模型完整时触发文本、双图顺序、严格 JSON 和非思考预验证；模型缺失时明确把真实契约留给 AG-03。
- [x] 检查仓库内无临时工程、下载缓存副本、权重或未解释文件。

验证证据：

- `pwsh -NoProfile -File scripts/tests/AG00.Tests.ps1`：`26/26` 通过。
- 真实 `scripts/setup.ps1 -Json`：进程与报告均返回 `1`；`12` 项通过、`4` 个模型文件缺失、`3` 项 warning、`5` 项 deferred、`0` 项探测错误。
- 本机通过：Node `24.18.0`、pnpm `11.19.0`、Rust/Cargo `1.98.1`、MSVC host、WebView2 `152.0.4191.66`、WGC `IsSupported=true`、默认播放/录音端点、DWM/User32、llama.cpp build `10752` 及文件哈希。
- `run-llama.ps1 -DryRun -NoUi` 保持含空格和 `&` 的路径参数边界，包含 `--reasoning off` 和 `--no-ui`，不含远程模型参数。
- 2026-09-05 实际下载尝试：`hf-mirror.com:443` 连续连接超时；按用户授权切换 `huggingface.co:443` 后同样连续连接超时，未收到模型字节，未在仓库留下缓存或权重。
- 真实 Qwen/SenseVoice 文件完整性和 Qwen 预验证尚未执行，因此本单元保持 `partial`，真实契约门禁继续归 AG-03。

提交：`f717502 build: implement AG-00 environment and model checks`

### AG-01 最小桌面壳

状态：`verified`

- [x] 单 package React/Vite/Tauri v2 应用可以构建和启动。
- [x] `main.rs` 仅调用 `lib.rs::run()`，所有 command 均已注册。
- [x] pet、danmaku、control 三个窗口可按需创建并安全退出。
- [x] capability/permission 仅授予 MVP 所需权限。
- [x] 开发态和构建态均能从 `resource_dir` 定位外置资源。

验证证据：

- `pnpm install --frozen-lockfile --offline`、`pnpm build` 通过；Vitest `14/14` 通过。
- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo check --all-targets` 通过；Rust 单元测试 `18/18` 通过。
- `pnpm tauri build --no-bundle` 通过，release PE 为 `Windows GUI` subsystem `2`；生产可执行文件能从 `resource_dir` 找到 model/defaults 配置、Qwen preflight 和完整桌宠清单。
- release smoke 创建 control、pet、danmaku 三个窗口，四项资源均为 `true`，应用本体退出码 `0`，退出后无应用或本轮 WebView2 子进程残留。
- 真机窗口探针确认 pet/danmaku 均置顶且不抢焦点，danmaku 稳定态具有 layered/transparent click-through 扩展样式；桌宠实际渲染为固定 `320x480` 底部居中容器。
- release 默认启动仅显示桌宠且不改变前台 HWND；向唯一桌宠窗口发送正常关闭消息后，应用退出码 `0`，主进程和 6 个本轮 WebView2 后代进程均在有界时间内退出。
- 已知非阻塞诊断：快速 smoke 退出时 WebView2 记录一次 `Chrome_WidgetWin_0` unregister `Error=1412`；不影响退出码、窗口行为或进程清理，留待集成收口复核。

提交：`3840eb0 feat: implement AG-01 desktop shell`

### AG-02 运行时与隐私内核

状态：`verified`

- [x] 所有后台 channel 有界且队列满行为确定。
- [x] pause 能取消任务、清空缓冲并以 generation 拒绝晚到结果。
- [x] 三个功能开关遵循各自停止语义，不误伤已接受的直接输入。
- [x] 启动状态和设置 JSON 原子写入，损坏时保守启动。
- [x] 重复暂停、恢复、退出和模型故障不死锁、不 panic。

验证证据：

- `pnpm test`：Vitest `24/24` 通过，覆盖 IPC 错误脱敏、运行时事件修订、开关串行化、暂停动画和事件清理。
- `cargo test --manifest-path src-tauri/Cargo.toml`：Rust 单元及集成测试 `73/73` 通过；`core_flow` 覆盖各 lane 满载策略、来源开关 generation、100 轮暂停/恢复、损坏配置保守启动和 worker panic 后有界退出。
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check`、`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` 和 `git diff --check` 通过。
- `pnpm build` 与 `pnpm tauri build --no-bundle` 通过，release 可执行文件成功生成。
- Tauri capability 按 control、pet、danmaku 窗口拆分；danmaku 无 IPC 权限，pet 只拥有运行时读取、事件监听和自身/控制台窗口操作权限。
- AG-02 不需要真实模型或采集设备；Qwen、SenseVoice、WGC 和真实音频契约未在本单元执行。

提交：`cac1518 feat: implement AG-02 runtime privacy core`

### AG-03 提示词与 Qwen 适配

状态：`partial`

- [x] prompt/schema 只通过 Prompt ID 从资源目录加载。
- [x] 代码和测试中不存在模型提示词正文或 fallback prompt。
- [x] llama.cpp 启动、健康检查、请求、超时、停止和错误状态可测试。
- [x] fake 响应覆盖合法及各类非法输出。
- [ ] 真实模型通过 5/8 图、非思考、严格 schema 和图片顺序契约。

验证证据：

- `cargo test --manifest-path src-tauri/Cargo.toml`：Rust `94` 个常规测试通过，真实 `model_contract` 的 `1` 个用例按设计默认 ignored。
- `prompt_contract` 验证两个 Prompt ID、必需变量、路径边界、严格 schema、外置 prompt 唯一来源；fixture 覆盖合法 JSON、额外字段、Markdown 包裹、截断 JSON 和 reasoning 内容。
- fake llama 生命周期覆盖 HTTP 503 加载态、ready、模型列表、独立非思考 argv、图片顺序、single-flight 请求锁、超时、启动清理和停止；请求体不提供 reasoning/chat-template override，并使用 `repeat_penalty`。
- `pnpm build`、`pnpm test`（`28/28`）、`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `git diff --check` 通过。
- `pnpm tauri build --no-bundle` 通过；release 资源目录已核对包含 config、两类 prompt、默认 persona、preflight probe 和两个 schema。
- 显式执行 `cargo test --manifest-path src-tauri/Cargo.toml --test model_contract -- --ignored`：失败为 `ArtifactMissing`；本机缺少固定 Qwen GGUF/mmproj，未伪造真实模型通过。
- AG-03 开发期间复现一次 Windows 并发读取导致原子替换超过原 8 ms 重试窗口；仅对 sharing/access 错误把有界重试调整至约 128 ms，定向测试和完整门禁均通过。

提交：`5a6472e fix: harden atomic settings replacement`；`125df02 feat: implement AG-03 Qwen adapter contracts`；`79fad7a test: verify Qwen request single flight`

### AG-04 文字对话闭环

状态：`partial`

- [x] direct lane reservation、accepted 和唯一终态语义正确。
- [x] 文字任务经过共享 Qwen 仲裁器并优先于 observation。
- [x] 短期上下文按完整轮次淘汰，退出后清空。
- [x] 正常、未就绪、超时、schema 错误、取消和暂停都有明确 UI 状态。
- [x] pause 后的在途文字回复不能显示。

代码与 fake 验证证据：

- 已定义 accepted/replied/failed/cancelled DTO，终态协议只允许 replied、failed、cancelled。
- 已实现只保存在内存中的 `ChatHistory`；接收真实 tokenizer 的 token 数，在 80% 阈值触发后按完整最早轮次淘汰到约 70%，并支持退出清空。
- Supervisor 对 direct task 原子保留有界容量；取消回调在任务首次 poll 前也只执行一次，正常完成不会触发取消终态。
- `LlamaChatModel` 的 tokenize 与 request 共用同一个 Qwen single-flight 锁；direct admission 会清空并取消 observation，但关闭主动回复不会取消已经 accepted 的 direct work。
- 生产运行时加载外置 model config、prompt 和默认 persona，后台有界启动 llama.cpp，退出时中止未完成的启动、清空会话并有界停止模型；资源不可用时使用显式 unavailable 模型，不包含 fallback prompt/model。
- 已注册 `send_text_message`、`get_chat_history`、`clear_chat_history` 和 `chat-status`，control/pet capability 只增加这三个命令权限，danmaku 权限保持为空。
- control 和 pet 均接入 `MessageInput`/`SpeechBubble`；accepted、replied、model unavailable、timeout、invalid response、cancelled 均有固定 UI，pause/shutdown 清空显示，并按 `runtimeGeneration` 拒绝延迟旧事件。
- `cargo test --manifest-path src-tauri/Cargo.toml`：`94/94` 通过，其中 `chat::tests` 为 `13/13`；`pnpm test`：`28/28`；`pnpm build`、Clippy、fmt 和 `git diff --check` 通过。
- `pnpm tauri build --debug --no-bundle` 通过，生成 `src-tauri/target/debug/ai-jarvis.exe`；浏览器视觉核验覆盖 control `960x720`、`390x720` 和 pet `320x480`，修复了窄屏聊天区 flex 收缩导致的相邻区重叠。
- 显式执行真实 `model_contract` 仍失败为 `ArtifactMissing`。本机没有固定 Qwen GGUF/mmproj，无法证明真实成功回复，因此 AG-04 保持 `partial`，不标记 `verified`。

阶段提交：`46d3d72 feat: add AG-04 chat context core`；`ec6fcf6 feat: implement AG-04 chat runtime core`；`2a75b18 feat: wire AG-04 chat through desktop UI`

### AG-05 屏幕观察与主动响应

状态：`in_progress`

- [ ] WGC 单显示器采集排除应用自身和用户配置的窗口。
- [ ] 无效帧、小变化和持续运动不会触发模型调用风暴。
- [ ] 帧环最多 8 张，observation 只保留最新事件，Qwen 保持 single-flight。
- [ ] fake clock 固定负载下队列和缓冲保持硬上限。
- [ ] 屏幕开关、主动响应开关和隐私 generation 能拒绝旧结果。

### AG-06 系统声音与主动语音

状态：`not_started`

- [ ] 系统音频只能生成环境观察，不能进入 user role/direct_chat。
- [ ] 用户语音使用高优先级 ASR lane 并生成直接用户消息。
- [ ] 麦克风仅在 reservation 后启动并遵守 60 秒/2 MiB 上限。
- [ ] 所有成功、失败、取消、暂停和退出路径均无孤儿音频文件。
- [ ] 真实 SenseVoice 覆盖中文、英文、静音和命令式系统音频契约。

### AG-07 游戏弹幕与桌宠状态表现

状态：`not_started`

- [ ] game 回复严格校验为 3-5 条短消息，非法批次整体拒绝。
- [ ] 拆句、轨道避碰、有界等待和取消清理有单元测试。
- [ ] 动画在固定容器内切换，GIF 可以从第一帧重播。
- [ ] overlay 置顶、透明、鼠标穿透且不抢焦点。
- [ ] 离开 game、关闭主动响应或暂停时立即清空弹幕。

### AG-08 MVP 集成收口

状态：`not_started`

- [ ] 所有常规测试、真实模型契约和真机 smoke 通过。
- [ ] 固定负载下队列、内存缓冲和临时文件回到预期上限。
- [ ] 全部 MVP 闭环连续切换时无陈旧回复或来源串线。
- [ ] 打包构建可以加载全部外置资源，资源缺失时明确失败。
- [ ] README 记录启动、依赖、验证命令和已知限制。
- [ ] diff 中没有课程、SQLite、CI、签名、更新器等 MVP 外实现。

## 全局验证门禁

```powershell
pnpm build
pnpm test
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --test model_contract -- --ignored
cargo test --manifest-path src-tauri/Cargo.toml --test asr_contract -- --ignored
```

固定负载和真机 smoke 仍以核心开发计划第 9 节为准。在相应工程和测试尚未创建前，
命令保持未执行状态，不能据此宣称通过。

## 范围约束

MVP 阶段不预建课程、长期记忆、每日总结、SQLite、CI、签名、更新器、GUI 模型安装器或多 crate workspace。
只有核心开发计划第 11 节定义的真实触发条件出现后才扩展。
