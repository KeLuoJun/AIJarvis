# AI Jarvis 开发清单

本文件只记录可由代码、测试或真机验证证明的状态，不使用百分比。设计范围以
`AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md` 为产品目标，以
`AI_JARVIS_CORE_DEVELOPMENT_PLAN.md` 的 MVP 边界和依赖顺序为当前执行依据。

## 当前状态

- 当前工作单元：`AG-01 最小桌面壳`
- 状态：`in_progress`
- 下一依赖单元：`AG-02 运行时与隐私内核`
- 当前外部阻断：Qwen GGUF、mmproj 和 SenseVoice ONNX 尚未安装；不阻断 AG-01/AG-02，但 AG-03 真实模型契约保持未通过
- 模型下载源：优先 `https://hf-mirror.com`；镜像明确失败后按用户授权切换到固定 revision 的 `https://huggingface.co`
- 最近审计基线：`f717502 build: implement AG-00 environment and model checks`

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

状态：`not_started`

- [ ] 单 package React/Vite/Tauri v2 应用可以构建和启动。
- [ ] `main.rs` 仅调用 `lib.rs::run()`，所有 command 均已注册。
- [ ] pet、danmaku、control 三个窗口可按需创建并安全退出。
- [ ] capability/permission 仅授予 MVP 所需权限。
- [ ] 开发态和构建态均能从 `resource_dir` 定位外置资源。

### AG-02 运行时与隐私内核

状态：`not_started`

- [ ] 所有后台 channel 有界且队列满行为确定。
- [ ] pause 能取消任务、清空缓冲并以 generation 拒绝晚到结果。
- [ ] 三个功能开关遵循各自停止语义，不误伤已接受的直接输入。
- [ ] 启动状态和设置 JSON 原子写入，损坏时保守启动。
- [ ] 重复暂停、恢复、退出和模型故障不死锁、不 panic。

### AG-03 提示词与 Qwen 适配

状态：`not_started`

- [ ] prompt/schema 只通过 Prompt ID 从资源目录加载。
- [ ] 代码和测试中不存在模型提示词正文或 fallback prompt。
- [ ] llama.cpp 启动、健康检查、请求、超时、停止和错误状态可测试。
- [ ] fake 响应覆盖合法及各类非法输出。
- [ ] 真实模型通过 5/8 图、非思考、严格 schema 和图片顺序契约。

### AG-04 文字对话闭环

状态：`not_started`

- [ ] direct lane reservation、accepted 和唯一终态语义正确。
- [ ] 文字任务经过共享 Qwen 仲裁器并优先于 observation。
- [ ] 短期上下文按完整轮次淘汰，退出后清空。
- [ ] 正常、未就绪、超时、schema 错误、取消和暂停都有明确 UI 状态。
- [ ] pause 后的在途文字回复不能显示。

### AG-05 屏幕观察与主动响应

状态：`not_started`

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
