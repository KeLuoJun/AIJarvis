# AI Jarvis MVP 核心开发计划

版本：v3.0（Agent 执行版）
日期：2026-09-05
依据：[AI Jarvis 开发设计文档](AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md)
状态：MVP Agent 实施基线

## 1. 复杂度评估结论

上一版结构对完整 v1 和公开发布是合理的，但对当前 MVP 明显过度设计。它提前引入了多 Cargo crate、CI/发布流水线、ADR、系统级 E2E、SQLite migration、恢复 job、OCR/embedding 算法和完整数据生命周期。这些内容会增加初期理解、编译、联调和维护成本，却不能更快证明 AI Jarvis 的核心体验是否成立。

本版采用以下原则：

1. 只创建当前 MVP 会运行或测试的文件，不创建未来功能的空目录。
2. 先用一个 Node 包、一个 Rust crate、一个 Vite 入口和少量 Rust 模块完成闭环。
3. 只有当真实代码出现独立发布、崩溃隔离、依赖冲突或明显复用需求时才拆 crate。
4. 完整设计文档仍是产品方向，但 MVP 不宣称满足完整首期发布标准。
5. 开发进度只由依赖和验收证据决定，不按人数、工期或日历日期估算。

### 1.1 哪些复杂度现在删除

| 原计划内容 | MVP 决策 | 原因 |
|---|---|---|
| `.github/workflows/*` | 不创建 | 不影响本地开发和运行；需要远程重复验证或准备发布时再加 |
| 根 Cargo workspace 和多个业务 crate | 不创建 | 当前只有一个桌面应用，模块边界足够 |
| 独立 protocol/ports/storage/inference crate | 不创建 | 先在单 crate 内用 Rust 模块和少量 trait 隔离 |
| 独立 ASR worker 工程 | 暂不创建 | 先验证进程内 SenseVoice；出现 native 崩溃或 DLL 冲突再拆 |
| SQLite、migration、后台恢复 job | 不创建 | MVP 不保存课程、活动、记忆等关系数据 |
| OCR、OpenCLIP、DINOv2、PELT、Binseg、K-medoids | 不创建 | 都服务于课程或每日总结，不是实时陪伴闭环前置条件 |
| region selector 窗口 | 不创建 | MVP 先支持按应用/窗口排除；区域框选后续补 |
| architecture/ADR/operations 文档体系 | 不创建 | 当前只维护设计文档、MVP 计划和 README |
| WinGet GUI 安装器、模型下载管理器 | 不创建 | 内部 MVP 使用 setup 脚本和明确前置条件 |
| 签名、自动更新、正式安装升级 | 不创建 | 公开分发前再实现 |
| 长时间 soak 与完整硬件矩阵 | 延后 | MVP 先用固定负载计数验证资源上限 |

### 1.2 不能为了简单而删除的边界

以下内容直接决定核心功能是否正确，MVP 必须保留：

- 所有模型提示词从 `resources/prompts` 加载，代码中不出现提示词正文或 fallback prompt。
- Qwen3.5-4B 必须以非思考模式运行，模型输出经过严格 JSON 校验。
- Rust 是采集、调度、隐私状态和模型调用的唯一权威所有者。
- 屏幕帧、系统音频、消息队列和聊天上下文全部有硬上限。
- `system_audio` 与 `user_voice` 是不同类型，系统声音永远不能成为用户指令。
- 隐私暂停会停止新采集和新模型提交，并让所有旧 generation 结果失效。
- WebView 不获得任意文件系统、shell 或 llama.cpp HTTP 权限。

## 2. MVP 范围

MVP 要验证的是“本地多模态桌宠是否真的有连续陪伴价值”，不是一次完成设计文档中的所有数据产品。

### 2.1 五条必须打通的闭环

```text
文字输入
-> Qwen 非思考回复
-> 桌宠气泡

屏幕有意义变化
-> 最近 5-8 张有序画面
-> Qwen 场景/响应判断
-> 普通气泡或保持沉默

用户主动麦克风
-> SenseVoice
-> DirectUserMessage
-> Qwen 必答
-> 桌宠气泡

系统声音
-> SenseVoice
-> ObservationTranscript
-> 仅作为当前观察上下文

game 场景
-> 3-5 条消息
-> 透明弹幕
-> 离场或暂停立即清空
```

上述任一流程中触发隐私暂停，都必须停止采集、拒绝新模型任务、清空气泡/弹幕，并丢弃暂停前的在途结果。

### 2.2 MVP 包含

- Windows 单显示器支持。
- Tauri v2 + React + TypeScript + Vite。
- 一个前端 bundle，根据 Tauri window label 渲染桌宠、弹幕或控制窗口。
- 一个默认文件化人设，MVP 不做人设 CRUD。
- 本地 Qwen3.5-4B 多图理解、文字对话和主动响应。
- WGC 屏幕采集、基本有效性判断、pHash 变化检测和有界帧环。
- WASAPI 系统声音、用户主动麦克风、VAD 和 SenseVoice 转写。
- 待机、普通互动和隐私暂停三种桌宠动画。
- 游戏弹幕、基本轨道避碰、点击穿透和不抢焦点。
- 屏幕感知、系统声音、主动响应三个开关，以及隐私总暂停。
- 当前进程内的短期聊天上下文。
- 原子 JSON 设置和暂停状态。
- 本地开发构建与关键自动化测试。

### 2.3 MVP 不包含

- 自动/手动课程、课程状态机、OCR、课程图片和笔记生成。
- 长期记忆、正式活动帧、每日总结、向量检索。
- SQLite、数据导出、复杂删除图和崩溃恢复工作流。
- 多人设管理和运行时人设切换。
- 屏幕区域框选、多显示器专项适配。
- 静止 2 分钟后的三轮强制互动策略。
- GUI 模型安装器、断点下载 UI、安装升级和自动更新。
- 云端推理、TTS、唤醒词和持续麦克风监听。

课程是 MVP 后的第一个完整功能包。课程不能先做“半套”，因为它必须同时具备 OCR、音频持久化、关键帧、笔记生成、原子提交和失败恢复，否则会产生不可接受的数据丢失风险。

## 3. MVP 工程方案

### 3.1 架构

```text
React 单 bundle
  pet | danmaku | control
              |
              | Tauri commands / events
              v
Rust AppRuntime
  state | privacy | supervisor | scheduler
       |           |             |
       v           v             v
 WGC/WASAPI    llama.cpp      SenseVoice
 屏幕/音频      Qwen HTTP      本地 ASR
```

只有 llama.cpp 是独立进程。SenseVoice 在 MVP 中先放在 Tauri Rust 进程内，减少 worker、wire protocol、sidecar 打包和第二套生命周期管理。AG-00 或 AG-06 如果复现 native crash、DLL 冲突或无法可靠取消，则立即改为同一 Cargo package 下的 `src-tauri/src/bin/asr_worker.rs`；在风险出现前不预建。

### 3.2 模块边界

- `lib.rs` 只负责 Tauri Builder、状态注册、窗口初始化和 `generate_handler![]`。
- `commands.rs` 只做参数校验、调用 Rust 服务和返回 DTO，不放业务流程。
- `supervisor.rs` 管理长任务、Qwen/ASR 优先级、有界 channel、取消令牌和退出顺序。
- `privacy.rs` 管理总暂停、generation 和最终提交检查。
- `scheduler.rs` 管理采样节奏、显著变化、Qwen 单飞和最新事件合并。
- `llama.rs` 管理 llama.cpp 进程、健康检查和 OpenAI-compatible 请求。
- `prompts.rs` 只按 Prompt ID 从资源目录加载、渲染和校验模板。
- `screen.rs`、`audio.rs`、`asr.rs` 封装 Windows/模型细节。
- `settings.rs` 只保存 MVP 设置和暂停状态，不演化成通用 repository。

不引入依赖注入框架。需要 fake 的边界用一个小 trait 即可，trait 与使用它的模块放在一起。

### 3.3 MVP 状态

| 状态 | 值 | 说明 |
|---|---|---|
| `privacy_state` | active / paused | 优先级最高并持久化 |
| `model_state` | unavailable / starting / ready / busy / degraded | llama.cpp 状态 |
| `scene` | game / course / meeting / video / coding / reading / work / chat / other | 只描述当前画面；MVP 不启动课程会话 |
| `interaction_state` | idle / responding / recording / processing | 当前交互 |
| `screen_enabled` | true / false | 独立开关 |
| `system_audio_enabled` | true / false | 独立开关 |
| `proactive_response_enabled` | true / false | 独立开关 |

桌宠显示优先级简化为：

```text
paused > interacting > idle
```

## 4. MVP 实际项目结构

下面是 MVP 开发期间实际创建的结构。它不是远期完整结构，不应预先补课程、记忆、CI 或发布目录。

```text
AIJarvis/
├── index.html
├── package.json
├── pnpm-lock.yaml
├── tsconfig.json
├── vite.config.ts                       # Vite 与 Vitest 共用配置
├── public/
│   └── assets/
│       └── pet/
│           ├── manifest.json
│           ├── idle.gif
│           ├── idle.png
│           ├── normal.gif
│           ├── normal.png
│           ├── closed-eyes.gif
│           └── closed-eyes.png
├── src/
│   ├── main.tsx                         # 唯一 Vite 入口
│   ├── WindowRoot.tsx                   # 按 window label 选择界面
│   ├── windows/
│   │   ├── PetWindow.tsx
│   │   ├── DanmakuWindow.tsx
│   │   └── ControlWindow.tsx
│   ├── components/
│   │   ├── PetSprite.tsx
│   │   ├── SpeechBubble.tsx
│   │   ├── MessageInput.tsx
│   │   ├── MicrophoneButton.tsx
│   │   └── PrivacyToggle.tsx
│   ├── lib/
│   │   ├── ipc.ts                      # invoke、事件订阅和清理
│   │   ├── types.ts                    # 少量前端 DTO
│   │   ├── danmaku.ts                  # 拆句、轨道和发射调度
│   │   └── danmaku.test.ts             # 拆句、避碰和取消
│   └── styles.css
├── src-tauri/
│   ├── Cargo.toml
│   ├── Cargo.lock
│   ├── build.rs
│   ├── tauri.conf.json                 # 含 resources 打包映射
│   ├── capabilities/
│   │   └── default.json                # 最小窗口/event/custom command 权限
│   ├── permissions/
│   │   └── app.toml                    # MVP commands，不授予通用 shell/fs
│   ├── icons/
│   │   └── icon.ico
│   ├── src/
│   │   ├── main.rs                     # 仅调用 ai_jarvis::run()
│   │   ├── lib.rs                      # Builder、State、generate_handler!
│   │   ├── commands.rs
│   │   ├── state.rs
│   │   ├── types.rs
│   │   ├── error.rs
│   │   ├── supervisor.rs
│   │   ├── privacy.rs
│   │   ├── scheduler.rs
│   │   ├── screen.rs                   # WGC、有效帧、排除和 pHash
│   │   ├── audio.rs                    # WASAPI loopback 与主动麦克风
│   │   ├── asr.rs                      # MVP 进程内 SenseVoice
│   │   ├── llama.rs                    # 进程、health、HTTP 和 JSON 校验
│   │   ├── prompts.rs                  # Prompt ID -> 文件
│   │   ├── chat.rs                     # 短期上下文与直接对话
│   │   ├── windows.rs                  # 三个 Tauri 窗口
│   │   └── settings.rs                 # 两个原子 JSON 文件
│   └── tests/
│       ├── core_flow.rs
│       ├── prompt_contract.rs
│       ├── model_contract.rs
│       ├── asr_contract.rs
│       └── fixtures/
│           ├── audio/
│           ├── frames/
│           └── model_outputs/
├── resources/
│   ├── prompts/                        # 所有模型提示词唯一来源
│   │   ├── manifest.toml
│   │   ├── persona/
│   │   │   └── default.md
│   │   ├── observation/
│   │   │   ├── system.md
│   │   │   └── user.md.jinja
│   │   └── direct-chat/
│   │       ├── system.md
│   │       └── user.md.jinja
│   ├── schemas/
│   │   ├── observation.schema.json
│   │   └── direct-chat.schema.json
│   └── config/
│       ├── model.toml                  # llama/Qwen/SenseVoice 固定配置
│       └── defaults.toml               # 采样和队列上限
├── scripts/
│   ├── setup.ps1                       # 检查 Rust/Node/WebView2/llama/模型
│   └── run-llama.ps1                   # 读取固定配置启动开发 server
├── docs/
│   ├── AI_JARVIS_DEVELOPMENT_DESIGN_FINAL.md
│   └── AI_JARVIS_CORE_DEVELOPMENT_PLAN.md
├── .gitignore
└── README.md
```

### 4.1 为什么这些文件不能再大幅删除

- Tauri v2 需要 `Cargo.toml`、`build.rs`、`tauri.conf.json` 和 capability。
- `main.rs` 保持极薄、Builder 放在 `lib.rs`，可以避免命令注册和测试入口混乱。
- 一个 capability 和一个 permission 文件已经是可维护的最小权限配置。
- `resources/prompts` 与 `resources/schemas` 是用户明确要求，也是模型输出可靠性的基础。
- `supervisor/privacy/scheduler` 分开，是为了避免暂停竞态和无限队列，不是为了未来扩展。
- `screen/audio/asr/llama` 分开，是因为它们依赖不同的 Windows API 或模型运行时。
- `danmaku.test.ts` 验证纯前端轨道算法；不为 MVP 引入 Playwright 或单独 E2E 工程。

### 4.2 何时才允许继续拆分

满足下列任一条件再拆目录或 crate：

- 单个 Rust 文件长期超过约 500 行且包含两个可独立测试的职责。
- ASR native 库能复现主进程崩溃或 DLL 冲突，需要 worker 隔离。
- 出现第二个应用、第二个客户端或需要独立发布的二进制。
- IPC DTO 数量明显增长并多次发生 Rust/TypeScript 漂移。
- 某模块需要独立重启、独立打包或引入与主进程不兼容的依赖。

仅因为“以后可能需要”不能成为拆分理由。

## 5. MVP 本地数据

MVP 不引入 SQLite。

```text
<tauri_app_config_dir>/
├── startup-state.json                  # privacy_paused、schema_version
└── settings.json                       # 三个功能开关、排除应用/窗口

<tauri_app_cache_dir>/
└── microphone/
    └── <recording-id>.wav              # 仅录音期间存在
```

规则：

- `startup-state.json` 与 `settings.json` 都使用临时文件 + flush + rename 原子更新。
- 首次运行默认 paused，由用户在控制窗口明确启用。
- 暂停状态文件损坏或不可读时按 paused 启动。
- `settings.json` 损坏时使用保守默认值，并显示诊断。
- 短时屏幕帧、系统音频环、转写、聊天上下文和模型回复不落盘。
- 麦克风录音立即转换为 16 kHz 单声道 PCM，单次最多 60 秒或 2 MiB，任一上限到达即停止并提交。
- 应用启动后、启动任何采集前，幂等清空 `cache/microphone` 中的孤儿文件；不为此引入恢复 job。
- 麦克风音频在取消、失败、成功转写、暂停或正常退出后删除。
- 模型文件在 MVP 中由 setup 脚本或 llama.cpp/Hugging Face 缓存管理，应用不维护第二份模型仓库。

## 6. 提示词与模型

### 6.1 提示词文件化

MVP 只有两个 Qwen task：

| Prompt ID | 用途 | 输出 |
|---|---|---|
| `observation` | 有序多图、场景、系统声音上下文和是否主动回复 | `observation.schema.json` |
| `direct_chat` | 用户文字或主动语音直接对话 | `direct-chat.schema.json` |

`resources/prompts/manifest.toml` 只记录 Prompt ID、system/user 文件、schema、必需变量、最大输出 token 和采样配置。MVP 不实现共享 partial、递归 include、版本 registry 或复杂 hash 图；当提示词数量超过约 5 个或准备发布时再增加。

强制规则：

- Rust、TypeScript、PowerShell 和测试代码中不得出现模型角色、语气、观察或总结指令正文。
- 代码只能引用 Prompt ID 和结构化变量。
- prompt、schema 或变量缺失时任务失败并显示诊断，禁止硬编码 fallback。
- 产品固定文案不是模型提示词，可以存在于前端组件中。
- 两类任务都只接受单个 JSON object；Markdown 包裹、额外字段和截断 JSON 均拒绝。
- `tauri.conf.json > bundle.resources` 显式包含 `resources/prompts`、`resources/schemas` 和 `resources/config`；运行时只通过 Tauri `resource_dir` 解析并校验规范化后的路径。

示例 manifest：

```toml
[[prompt]]
id = "observation"
system = "observation/system.md"
user = "observation/user.md.jinja"
schema = "../schemas/observation.schema.json"
required_variables = ["persona", "runtime_state", "audio_observations"]
max_output_tokens = 384
sampling_profile = "observation_non_thinking"
```

### 6.2 Qwen3.5-4B 非思考模式

当前开发机验证版本：

```text
llama.cpp 0.3.0-dev
build 10752
commit b96806d96
```

模型服务验证命令：

```powershell
llama serve `
  -hf 'unsloth/Qwen3.5-4B-GGUF:UD-Q4_K_XL' `
  --reasoning off `
  --alias 'qwen3.5-4b' `
  --host 127.0.0.1 `
  --port 8080
```

该命令保留 llama.cpp 默认 WebUI，便于 AG-00 验证。当前构建中 `--reasoning off` 已同时关闭 reasoning 和模板 thinking；额外传 `--chat-template-kwargs '{"enable_thinking":false}'` 会产生弃用警告，MVP 不再使用该冗余参数。

应用启动模型时使用独立 argv，不拼接 shell 字符串，并额外使用 `--no-ui`。`model.toml` 固定 llama.cpp build/commit、可执行文件、精确 GGUF/mmproj/SenseVoice 文件、SHA-256、alias、host、port、context 和采样参数；运行 argv 使用校验后的本地 `--model` 与 `--mmproj` 路径。MVP 不兼容旧 `llama-server.exe` 分支。

Rust adapter 还必须：

- 移除调用方提供的 reasoning、reasoning effort 和 chat template override；应用请求不发送这些字段，统一继承 server 的 `--reasoning off`。
- 把 Qwen 文档中的 `repetition_penalty` 映射为 llama.cpp 的 `repeat_penalty`。
- 将 `/health` 的 HTTP 503 视为加载中；只有 HTTP 200 且 `status=ok` 才 ready。
- 启动后验证 `/v1/models`、多图请求、JSON schema 和无 `<think>`/`reasoning_content`。

### 6.3 MVP 模型安装边界

- `scripts/setup.ps1` 检查固定 llama.cpp build，并校验 Qwen GGUF/mmproj 和 SenseVoice 文件名、大小及 SHA-256。
- 控制窗口显示“未安装、加载中、可用、失败”，但不负责下载进度、断点续传或系统安装。
- 允许配置已存在的模型路径；不能静默改用另一个量化或 latest。
- 准备给非技术用户测试前，再实现 GUI 安装器和完整模型 bundle 校验。

## 7. 核心运行规则

### 7.1 Supervisor 与隐私暂停

`Supervisor` 管理屏幕、系统声音、麦克风、ASR、调度器和 llama 请求。每条后台路径都有：

- 有界 channel。
- cancellation token。
- `runtime_generation`。
- 明确停止顺序。
- 队列满时的丢弃规则。

Qwen 和 ASR 各自只有一个共享仲裁点，不允许模块绕过它直接调用模型：

```text
Qwen direct lane（文字、用户语音）    -> 预留容量、不可被 observation 淘汰
Qwen observation lane（屏幕、系统声音） -> 只保留最新事件、允许合并和丢弃

ASR user_voice lane   -> 高优先级，开始录音前预留容量
ASR system_audio lane -> 低优先级，队列满时丢弃最旧环境段
```

所有 Qwen task 共用一个 single-flight 执行器。新的 direct task 会清除等待中的 observation，并取消当前 observation 的客户端请求；即使底层推理未立即停止，其返回结果也会丢弃，direct task 获得下一次执行权。

暂停顺序：

```text
Rust 接受 pause
-> paused=true，generation+1
-> 关闭新截图、音频、麦克风和模型任务准入
-> cancel 在途任务
-> 清内存帧、音频和待处理事件
-> 清空气泡和弹幕
-> 原子写 startup-state.json
```

旧任务即使无法立即中止，返回时也必须因 generation 不匹配而丢弃。解除暂停只恢复用户原有的三个独立开关，不自动全部开启。

### 7.2 屏幕与 Qwen 调度

MVP 使用单显示器和简单确定性过滤：

```text
WGC 每 3 秒采样
-> 排除自身窗口和用户配置的应用/窗口
-> 拒绝黑屏、纯色和损坏帧
-> 缩放并写入最多 8 帧内存环
-> pHash/前台应用变化
-> 到达显著事件或模型 deadline
-> Qwen 单飞
-> 最近 5-8 张有序图片
-> JSON schema 校验
-> 更新 scene/next_check
-> 展示或沉默
```

约束：

- 普通观察调用开始间隔不得小于 5 秒。
- Qwen 返回的下一次检查时间限制在 5-18 秒。
- 推理期间继续采样，但待处理事件最多保留一个最新值。
- 持续运动不等于每帧调用；鼠标和小动画不能形成事件风暴。
- 解析失败时保持沉默并使用默认间隔，不无限重试。
- MVP 不实现 OCR、语义向量和高级静止强制响应。

### 7.3 文字与短期上下文

- 文字输入只有取得 direct lane 的有界队列 reservation 后才返回 accepted。
- direct task 始终经共享 Qwen 仲裁器提交，优先于等待中的 observation。
- accepted 输入最终只能进入 `replied`、`failed` 或 `cancelled` 一个终态。
- 模型正常且未取消时必须返回经 schema 校验的回复。
- 模型不可用或有限重试耗尽时显示失败，不伪造回复。
- 历史只存在内存，达到 token 上限时按完整“用户 + 助手”轮次删除最旧内容。
- 退出程序后聊天历史清空。

### 7.4 系统声音与主动语音

```text
WASAPI loopback -> SystemAudioSegment -> ASR -> ObservationTranscript
主动麦克风      -> UserVoiceSegment  -> ASR -> DirectUserMessage
```

- 两种来源使用 Rust enum，不能互相转换。
- `ObservationTranscript` 只进入当前 observation 的不可信环境字段，不创建 user role。
- `DirectUserMessage` 走 direct_chat，模型正常时必须回复。
- 麦克风只在 direct ASR lane 预留成功后由按钮开启，再次点击结束，Esc 取消。
- 麦克风采集统一为 16 kHz 单声道 PCM，单次最多 60 秒或 2 MiB；系统声音段不能占用该预留容量。
- 转写正文不在 UI 中展示、编辑或确认。
- 系统音频使用最多 60 秒内存环，音频段数量和单段长度都有上限。
- 麦克风临时文件在 ASR 得到结果或终止后立即删除。

### 7.5 桌宠与游戏弹幕

- 待机显示 `idle.gif/png`。
- 普通回复播放一次 `normal.gif`，随后回到 `idle.png`。
- 隐私暂停播放一次 `closed-eyes.gif`，随后停在 `closed-eyes.png`。
- 动画和静态图使用同一个固定尺寸容器，避免窗口跳动。
- game 回复必须得到 3-5 条短消息，否则整批不展示。
- 弹幕在顶部约 30% 区域从右向左移动，发射间隔 0.4-1.2 秒。
- 使用固定轨道和文本宽度估算避免重叠；无安全轨道时有界等待。
- 弹幕窗口置顶、透明、鼠标穿透且不抢焦点。
- 离开 game、关闭主动响应、暂停或退出时清除待发和在途弹幕。

### 7.6 三个功能开关的停止语义

- 关闭屏幕感知：停止 WGC，清空帧环和待处理观察，取消或失效含屏幕的在途 observation。
- 关闭系统声音：停止 loopback，清空音频环和环境转写，取消或失效该来源等待中或在途的 ASR/observation。
- 关闭主动响应：取消等待中或在途的 observation 回复并清空普通气泡/弹幕；不停止采集，也不得取消已经 accepted 的文字或用户语音。
- 每个开关操作都递增对应 source generation；最终提交同时校验全局 privacy generation 和 source generation。

## 8. Agent 执行计划

本计划不使用工期、人数或日历排期。`AG-00` 至 `AG-08` 是有前置条件和验收门禁的工作单元，不代表天数或周数。文档中的秒数只描述产品运行规则，例如采样间隔、模型 deadline 和录音上限，不用于估算开发进度。

### 8.1 Agent 执行协议

Agent 每次开发按同一协议执行：

1. 读取设计文档、本计划、仓库指令、当前 `git status`、相关源码和测试，不根据目录名猜测完成状态。
2. 在依赖已满足的工作单元中选择编号最小的未完成单元；完成状态必须由代码和验证结果证明。
3. 修改前列出本单元的目标文件和不在范围内的内容，保留用户已有改动。
4. 先实现一条可运行的纵向闭环，再补同一单元内的错误路径；不创建空模块、占位 API 或仅写 `TODO` 的未来结构。
5. 新增行为必须在同一单元增加相应测试或可重复的真机验证步骤。
6. 先运行最小相关测试，再运行本单元要求的完整门禁，最后检查 diff、未跟踪文件和 prompt 硬编码。
7. 门禁未通过时不得把单元标记为完成，也不得用删除断言、放宽 schema、无限重试或硬编码 fallback 绕过失败。
8. 如果外部模型、驱动或设备阻断验证，保留可复现命令、原始错误和已排除原因；其他不依赖该阻断的单元仍可继续。

默认由一个 Agent 按编号推进。只有两个已就绪单元的目标文件不重叠、共享契约已固定且验证互不占用同一模型/设备时，才允许并行执行；并行不是完成要求。

### 8.2 工作单元依赖

```text
AG-00 基线与外部契约
  |
  v
AG-01 最小桌面壳
  | \
  |  +----> AG-03 提示词与 Qwen 适配
  v                  |
AG-02 运行时与隐私 --+
                     v
                 AG-04 文字对话闭环
                   |             \
                   v              v
            AG-05 屏幕观察    AG-06 音频与语音
                   |              |
                   +----> AG-07 游戏弹幕与状态表现
                                  |
                                  v
                           AG-08 MVP 集成收口
```

图只表示依赖。若 AG-00 发现真实模型暂不可用，AG-01 和 AG-02 可以继续；AG-03 只能完成代码与 fake 测试，真实模型契约门禁仍保持未通过，因此 AG-04 不能宣称完整完成。

### 8.3 AG-00：基线与外部契约

目标：把不可由应用代码控制的版本、模型和 Windows 能力变成可重复检查的输入。

主要落点：

- `resources/config/model.toml`
- `resources/config/defaults.toml`
- `scripts/setup.ps1`
- `scripts/run-llama.ps1`
- `.gitignore`

实现内容：

- 检查 Node、pnpm、Rust、WebView2、llama.cpp、Qwen GGUF/mmproj 和 SenseVoice 文件。
- 记录并校验 llama.cpp build/commit、文件路径、大小和 SHA-256，不接受 `latest` 漂移。
- 用独立 argv 生成开发启动参数，必须包含 `--reasoning off`，不得包含已弃用的 `enable_thinking` 参数。
- 若模型文件已存在，预执行最小 Qwen 文本、多图、严格 JSON、无 `<think>`/`reasoning_content` 验证；正式模型门禁仍归 AG-03。
- 探测 WGC、WASAPI loopback、麦克风和透明点击穿透窗口是否可用；探测代码必须进入后续正式模块或在结论记录后删除。

完成门禁：

- setup 对“依赖完整”和“依赖缺失”都给出确定、可操作且非零/零退出码正确的结果。
- `run-llama.ps1` 与文档中的非思考参数一致。
- 模型文件存在时记录预验证结果；缺失时 setup 必须准确列出缺失项，并把真实契约明确留给 AG-03，不能伪造通过。
- 没有遗留临时工程、下载缓存副本或未解释文件。

### 8.4 AG-01：最小桌面壳

依赖：AG-00 的工具链检查可执行；真实模型可以暂时不可用。

目标：建立能编译、能启动、能安全退出的单 package Tauri v2 应用。

主要落点：

- 根目录 Node/Vite 配置
- `src/main.tsx`、`src/WindowRoot.tsx`、三个 `src/windows/*`
- `src-tauri/Cargo.toml`、`build.rs`、`tauri.conf.json`
- `src-tauri/src/main.rs`、`lib.rs`、`commands.rs`、`windows.rs`
- `src-tauri/capabilities/default.json`、`permissions/app.toml`
- `public/assets/pet/*`

完成门禁：

- `pnpm build` 与 `cargo check --manifest-path src-tauri/Cargo.toml` 通过。
- `main.rs` 只调用 `lib.rs::run()`；所有 command 注册到 `generate_handler![]`。
- pet、danmaku、control 三个 window label 能按需创建；透明窗口不抢焦点且退出无残留进程。
- capability 只包含 MVP 需要的窗口、event 和自定义 command 权限，没有通用 shell/fs 权限。
- 开发态和构建态都能通过 Tauri `resource_dir` 找到外置资源。

### 8.5 AG-02：运行时与隐私内核

依赖：AG-01。

目标：在接入真实采集与模型前，先固定所有后台任务必须遵守的停止和失效语义。

主要落点：

- `src-tauri/src/state.rs`
- `src-tauri/src/types.rs`
- `src-tauri/src/error.rs`
- `src-tauri/src/supervisor.rs`
- `src-tauri/src/privacy.rs`
- `src-tauri/src/settings.rs`
- `src-tauri/tests/core_flow.rs`

完成门禁：

- fake capture/model/ASR 能证明所有 channel 有界且队列满行为确定。
- pause 原子地阻止新任务、取消在途任务、清空缓冲，并通过 generation 拒绝晚到结果。
- 三个独立开关按 7.6 节清理各自来源，主动响应关闭不影响已接受的直接输入。
- 两个 JSON 均原子写入；损坏状态以 paused 和保守设置启动。
- 重复执行暂停、恢复、退出和模型故障场景不死锁、不 panic。

### 8.6 AG-03：提示词与 Qwen 适配

依赖：AG-01；真实契约完成还依赖 AG-00 的模型可用门禁。

目标：建立唯一的模型调用入口，并保证非思考和结构化输出契约。

主要落点：

- `resources/prompts/*`
- `resources/schemas/*`
- `src-tauri/src/prompts.rs`
- `src-tauri/src/llama.rs`
- `src-tauri/tests/prompt_contract.rs`
- `src-tauri/tests/model_contract.rs`
- `src-tauri/tests/fixtures/model_outputs/*`

完成门禁：

- 两类 prompt 只能通过 Prompt ID 加载，变量缺失、路径越界或 schema 缺失时明确失败。
- Rust、TypeScript、PowerShell 和测试代码扫描不到模型提示词正文或 fallback prompt。
- llama.cpp 的启动、health、ready、请求、超时、停止和错误状态均可测试。
- fake 响应覆盖合法 JSON、额外字段、Markdown 包裹、截断 JSON 和思考内容。
- 真实 `model_contract` 通过 5/8 图、非思考、严格 schema 和图片顺序验证。

### 8.7 AG-04：文字对话闭环

依赖：AG-02、AG-03。

目标：交付第一个完整可用闭环“文字输入 -> Qwen -> 桌宠气泡”。

主要落点：

- `src-tauri/src/chat.rs`
- `src-tauri/src/commands.rs`
- `src/components/MessageInput.tsx`
- `src/components/SpeechBubble.tsx`
- `src/lib/ipc.ts`
- `src/lib/types.ts`

完成门禁：

- 输入只有获得 direct lane reservation 后才返回 accepted，并最终进入唯一终态。
- direct task 只能经过共享 Qwen 仲裁器，且优先于 observation。
- 短期上下文按完整轮次淘汰，退出后清空。
- 正常、模型未就绪、超时、schema 错误、取消和暂停都有明确 UI 状态。
- 在途文字回复经过 pause 后不能显示；模型故障不妨碍暂停和退出。

### 8.8 AG-05：屏幕观察与主动响应

依赖：AG-04。

目标：交付“屏幕变化 -> 有序多图 -> 场景判断 -> 气泡或沉默”闭环。

主要落点：

- `src-tauri/src/screen.rs`
- `src-tauri/src/scheduler.rs`
- `resources/prompts/observation/*`
- `resources/schemas/observation.schema.json`
- `src-tauri/tests/fixtures/frames/*`

完成门禁：

- WGC 只采集目标显示器，应用自身及排除窗口不进入帧环或模型请求。
- 黑屏、纯色、损坏帧、鼠标微动和小动画不会触发调用风暴。
- 帧环最多 8 张，observation 只保留最新事件，所有 Qwen task 仍由同一 single-flight 仲裁器执行。
- fake clock 下大量采样 tick 后，帧、事件和模型队列长度保持固定上限。
- 关闭屏幕感知、关闭主动响应或隐私暂停后，对应在途结果均因 source/global generation 失效。

### 8.9 AG-06：系统声音与主动语音

依赖：AG-04。

目标：交付来源严格隔离的系统声音观察和用户主动语音对话。

主要落点：

- `src-tauri/src/audio.rs`
- `src-tauri/src/asr.rs`
- `src/components/MicrophoneButton.tsx`
- `src-tauri/tests/asr_contract.rs`
- `src-tauri/tests/fixtures/audio/*`

完成门禁：

- `SystemAudioSegment` 只能生成 `ObservationTranscript`，不能进入 user role 或 direct_chat。
- `UserVoiceSegment` 使用高优先级 ASR lane，并转换为 `DirectUserMessage`。
- 麦克风只有在 reservation 成功后启动，遵守 60 秒/2 MiB 硬上限。
- 成功、静音、失败、Esc、暂停、退出和异常重启清理后都无孤儿麦克风文件。
- 真实 SenseVoice 契约覆盖中文、英文、静音和命令式系统音频。
- 若可复现 native crash、DLL 冲突或无法可靠取消，先增加 `src-tauri/src/bin/asr_worker.rs` 并重新通过本单元门禁。

### 8.10 AG-07：游戏弹幕与桌宠状态表现

依赖：AG-05；与 AG-06 完成集成后才能进入 AG-08。

目标：把 game 场景响应稳定呈现为桌面弹幕，并统一桌宠动画状态。

主要落点：

- `src/windows/PetWindow.tsx`
- `src/windows/DanmakuWindow.tsx`
- `src/components/PetSprite.tsx`
- `src/lib/danmaku.ts`
- `src/lib/danmaku.test.ts`
- `src/styles.css`

完成门禁：

- game 回复严格产生 3-5 条短消息，非法批次整体拒绝。
- 单元测试覆盖拆句上限、轨道避碰、无安全轨道等待和取消清理。
- 固定容器内动画切换不改变窗口尺寸，重复 GIF 能从第一帧重播。
- overlay 置顶、透明、鼠标穿透、不抢焦点；离开 game、关闭主动响应或暂停时立即清理。
- 代表性窗口化和无边框游戏场景通过 Agent 可观察的 UI/窗口状态验证；无法自动观察的限制写入 README。

### 8.11 AG-08：MVP 集成收口

依赖：AG-06、AG-07。

目标：证明所有核心闭环可以在同一运行时组合工作，并产出内部可运行构建。

主要落点：现有 MVP 文件；除修复集成问题外不新增架构目录。

完成门禁：

- 第 9 节所有常规测试、真实模型契约和真机 smoke 均通过。
- 加速时钟/fixture 场景完成固定次数的采样、模型排队、音频段、暂停恢复和弹幕发射，所有队列、内存缓冲和临时文件数量回到预期上限。
- 文字、屏幕、系统声音、用户语音、game 和隐私暂停按一组端到端场景连续切换，不出现陈旧回复或来源串线。
- 本地构建能从 `resource_dir` 加载全部 prompt/schema/config，卸载 prompt 后能力明确失败而非 fallback。
- `git diff` 中没有课程、SQLite、CI、签名、更新器或其他 MVP 外实现。
- README 记录启动命令、前置依赖、验证命令和已知限制；产物仅用于内部测试。

## 9. MVP 测试策略

不创建 `.github/workflows`。Agent 把以下命令作为本地完成门禁：

```powershell
pnpm build
pnpm test
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

只保留三类常规自动测试和两个需要本地模型的真实契约测试：

| 测试 | 覆盖 |
|---|---|
| `prompt_contract.rs` | prompt 文件存在、变量、严格 schema、无硬编码 fallback |
| `core_flow.rs` | 有界队列、single-flight、来源隔离、pause generation、accepted 终态 |
| `danmaku.test.ts` | 拆句数量、轨道避碰、无轨等待和取消清理 |
| `model_contract.rs` | 5/8 图、非思考、JSON、超时；默认 ignored |
| `asr_contract.rs` | 中英、静音、来源标记和超时；默认 ignored |

需要本地模型时显式执行：

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test model_contract -- --ignored
cargo test --manifest-path src-tauri/Cargo.toml --test asr_contract -- --ignored
```

真实环境 smoke 只覆盖：

- WGC 单显示器。
- 系统声音和主动麦克风。
- 桌宠透明窗口与动画。
- 一款代表性游戏 overlay。
- 应用启动、暂停、恢复、模型故障和完整退出。

资源稳定性不按运行时长声明通过，使用固定负载验证：

- fake clock 驱动至少 10,000 个屏幕采样 tick，帧环和 observation 队列不超过上限。
- 在模型请求、ASR 和弹幕各自处于在途状态时，分别执行至少 100 轮暂停/恢复，旧 generation 结果展示数必须为 0。
- 连续处理至少 1,000 个系统音频 fixture 段，ASR 环和临时文件数量最终回到基线。
- 连续调度至少 1,000 批弹幕，活跃轨道和待发队列不超过上限，取消后归零。

只有需要远程重复验证、分支保护或准备发布时，才把已经稳定的命令迁入一个 `ci.yml`；在此之前不设计 CI 矩阵。

## 10. MVP 完成定义

以下条件全部满足，才称为 MVP：

1. 应用可启动、隐藏/恢复并完整退出。
2. Qwen 以非思考模式工作，没有 `<think>` 或 reasoning content。
3. 所有模型提示词来自 `resources/prompts`，代码扫描无提示词正文。
4. 文字与有效用户语音在正常模型条件下必答。
5. 屏幕有意义变化可触发合法场景判断和适度主动气泡。
6. 系统声音只作为不可信环境观察，不能成为用户命令。
7. game 场景产生 3-5 条不抢焦点的弹幕。
8. 隐私暂停停止采集和新模型提交，清空 UI 并丢弃旧结果。
9. 屏幕、音频、事件、模型和聊天上下文均有可测试硬上限。
10. 除原子设置和录音期间的临时文件外，不持久化屏幕、音频、转写或聊天。
11. 打包构建能加载外置 prompt/schema/config，三个功能开关均能清理对应缓冲并拒绝旧结果。

## 11. 按真实需要再增加

| 扩展 | 触发条件 | 届时新增 |
|---|---|---|
| ASR worker | AG-00/AG-06 复现 native crash、DLL 冲突或需要独立重启 | MVP 内增加 `src-tauri/src/bin/asr_worker.rs`，必要时再拆 protocol crate |
| SQLite | 开始课程、记忆或活动持久化 | database、migration、repository |
| 课程功能包 | MVP 核心陪伴稳定，确认课程是下一优先级 | OCR、课程状态机、临时工作区、笔记 job、恢复测试 |
| 长期记忆 | 用户确认需要跨会话个性化 | candidate、approval、recall 和数据管理 |
| 每日总结 | 正式活动帧的隐私/保留策略已确定 | embedding、时序分段、后台总结和调度 |
| 区域选择器 | 应用/窗口排除不足以满足测试用户 | 临时透明 selector window 与独立 capability |
| GUI 模型安装 | 开始面向非技术测试者 | 下载、hash、进度、重试和磁盘预检 |
| 生成 TS bindings | IPC DTO 增长或发生两次以上漂移 | bindings 生成工具 |
| 多 crate workspace | 出现独立发布/依赖/所有权边界 | domain、inference、storage 等按真实边界拆分 |
| `.github/workflows` | 需要远程重复验证、主分支保护或准备发布 | 单一 CI 起步，按真实需求再拆 |
| ADR/operations 文档 | 决策反复引发回归或需要长期审计 | 只记录当时真实决策 |
| 签名/更新/安装器 | 准备公开发布 | Windows 签名、安装升级、updater、发布 E2E |

后续扩展遵循一条规则：先有功能和失败模式，再增加对应目录、抽象和流水线，不为假设中的未来预建结构。

## 12. Agent 单元交付记录

工作单元的状态不靠在本文维护百分比，也不按对话轮数计算。每次 Agent 完成或阻断一个单元时，在最终交付中提供以下记录，下一次执行再用仓库事实复核：

```text
工作单元：AG-xx
结果：verified | partial | blocked
变更文件：实际文件列表
验证证据：执行的命令、退出码、关键断言或 UI 状态
真实模型/设备：已验证项；未执行项及原因
范围检查：未新增的 MVP 外内容
剩余问题：可复现错误和下一步，不写模糊 TODO
下一就绪单元：根据依赖图计算；没有则写 none
```

判定规则：

- `verified`：本单元全部完成门禁通过。
- `partial`：已有可用实现，但至少一个门禁尚未通过；后续必须继续本单元，不能跳过依赖。
- `blocked`：缺少模型、驱动、硬件、权限或用户决策，且已有可复现证据；不得用 mock 冒充真实门禁。
- 测试、Git diff 和实际运行结果是最终事实；上一轮 Agent 的文字结论只能作为线索。

## 13. 参考资料

- [llama.cpp README](https://github.com/ggml-org/llama.cpp/blob/master/README.md)
- [llama.cpp server 文档](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
- [Unsloth Qwen3.5 运行指南](https://unsloth.ai/docs/models/qwen3.5)
- [unsloth/Qwen3.5-4B-GGUF](https://huggingface.co/unsloth/Qwen3.5-4B-GGUF)
- [Tauri v2 文档](https://v2.tauri.app/)

外部文档会变化。MVP 以 AG-00 实际验证并写入 `resources/config/model.toml` 的版本和参数为准。
