# YAAT

Yet Another Account Tool：面向 Codex、Claude Code 与 Claude Desktop 的本地多账号 / 多 Provider 管理工具，使用 Tauri 2 + React 19 + TypeScript + Rust 开发。前端基于官方 `create-tauri-app` 的 `react-ts` 模板，并采用 Tailwind CSS 4、shadcn/ui New York、Radix Primitives、Lucide 与 Recharts。

## 支持边界

| 客户端         | 隔离启动 | 实验性全局切换 | 本地用量 | 历史统一       |
| -------------- | -------- | -------------- | -------- | -------------- |
| Codex          | 支持     | 支持           | 支持     | 支持           |
| Claude Code    | 支持     | 支持           | 支持     | 支持           |
| Claude Desktop | 支持     | 支持           | 不支持   | 仅 Code 标签页 |

发布目标为 macOS 和 Windows。Linux 容器用于可重复构建与测试，不代表 Linux 桌面发布支持；Windows FFI、Credential Manager 和真实系统钥匙串故障路径仍需对应平台 CI。

## 第一版范围

- 支持 Codex、Claude Code 与 Claude Desktop 三个平台。
- 支持官方订阅账号、官方 API、原生协议兼容的第三方 Provider。
- 每个平台可保存多个配置并随时切换。
- 默认使用相互隔离的配置目录启动 CLI / Desktop，不污染默认账号。
- 三个平台的实验性全局切换都支持官方账号私有凭据；API / 第三方 Provider 使用各客户端的原生配置入口。
- 从 Codex / Claude Code 本机会话日志统计 Token 用量，可按日期和 IANA 时区过滤；Claude Desktop 用量解析暂未开放。
- Codex 与 Claude Code 可在全局目录和所有 YAAT 托管 Profile 之间统一本机会话历史；Claude Desktop 支持把不同账号 / 组织下的 Code 标签页历史统一到明确选择的目标分组。
- 不访问远程额度接口；远程配额与远程 Token 统计不在第一版范围内。
- 简体中文 / 英文 UI，支持浅色、深色和跟随系统。
- 通过签名的 GitHub Release 检查、下载并安装更新；下载可取消，安装完成后自动重启。

## 不丢配置的切换模型

YAAT 不会把数据库中的一整份配置序列化后覆盖 `config.toml` 或 `settings.json`。全局切换遵循以下约束：

1. 每个平台适配器声明一组精确到字段的归属路径。
2. 补丁引擎先解析当前文件并生成字段级变更，同时检查语义差异没有越出白名单。
3. 写入使用同目录临时文件与原子替换，避免程序退出或系统中断留下半个配置文件。
4. YAAT 第一次接管某个平台时，保存原始凭据和受管字段基线；后续账号切换只更新目标值，不覆盖原始基线。
5. “停止全局管理”只恢复 YAAT 拥有的账号字段及原始凭据；无关字段始终保留。单次切换失败则在当前进程内直接回滚。
6. YAAT 是单用户桌面工具，不为同权限恶意进程、伪造 IPC 或极端并发构造额外事务协议。

Codex 全局切换当前只接管：

- `model`
- `model_provider`
- `profile`
- `openai_base_url`
- `chatgpt_base_url`
- `base_url`
- `wire_api`
- `experimental_bearer_token`
- `model_providers.yaat_managed_v1`

`cli_auth_credentials_store` 在全局切换中只读取、不改写；用户选择的 `file`、`keyring` 或 `auto` 会原样保留。仅 YAAT 自己生成的隔离 `CODEX_HOME` 会明确使用 `file`，让每个隔离 Profile 拥有独立的 `auth.json`。

Claude Code 的 `settings.json` 当前只接管 `apiKeyHelper` 以及 `env` 中的 Anthropic / Claude Provider 选择字段。`permissions`、MCP、插件、主题、沙箱规则、用户注释和其他未知字段都不属于 YAAT。官方账号快照也不是整份 secure storage：只包含 `claudeAiOauth`、`organizationUuid`、`trustedDeviceToken`、`enterpriseGateway` 四个账号字段；`pluginSecrets`、`mcpOAuth`、`mcpXaaIdp`、`designOauth`、`gatewayTrust` 和未来未知字段既不会被旧账号覆盖，也不会被复制进 YAAT 数据库。

Claude Desktop 的官方账号按 Profile 设置独立 `CLAUDE_USER_DATA_DIR`。保存或导入账号时，YAAT 读取 `lastKnownAccountUuid`，通过 Electron Safe Storage 解密两套 OAuth Token Cache 和认证 Cookie，并把可回显、可复制的账号快照保存到本地数据库；Local Storage、IndexedDB、主题、扩展和其他 Cookie 不进入数据库。恢复账号时，YAAT 使用目标 Profile 的 Electron Safe Storage 重新加密 Token Cache 和 Cookie。全局切换只替换这些账号字段，停止管理时恢复首次接管前的状态。第三方 Provider 使用 Desktop 原生 3P config-library：YAAT 只管理 `deploymentMode`、自己的固定 library 条目与 `appliedId`。Desktop 会清理 helper 子进程环境，因此 YAAT 为每个 Provider 生成一个 `0700`、不含密钥的私有 wrapper，只携带 Profile ID 并调用 YAAT 凭据助手。第一版仅支持原生 Anthropic Messages 直连和 Desktop 可识别的 `claude-sonnet-*` / `claude-opus-*` / `claude-haiku-*` / `claude-fable-*` 路由；需要 OpenAI / Gemini 转换或模型映射的 Provider 会明确拒绝。

更完整的切换与恢复设计见 [docs/architecture.md](docs/architecture.md)。

## 凭据与数据库

- API Key、Token、官方账号快照和全局恢复基线直接保存在 YAAT 的本地 SQLite 数据库中，不再使用 YAAT 主密钥或系统钥匙串加密。
- 账号列表不返回凭据；打开单个账号的编辑框时，前端会读取并完整回显该账号的凭据，便于复制或替换。
- 官方账号创建既可以粘贴 YAAT 导出的凭据 JSON 后直接使用，也可以留空并在创建后登录。
- 前端没有文件系统、Shell、HTTP 或系统钥匙串权限；只能调用 capability 明确授权的 Tauri 命令。
- API Key / Token 通过 YAAT 自身的本地凭据助手按需输出给客户端，不写进客户端配置；Claude Desktop profile 只保存不含密钥的私有 helper wrapper 路径。

官方订阅账号的全局切换采用用户确认的“复制私有凭据”方案：

- Codex 支持 `file`、`keyring` 与 `auto`；全局切换不会擅自改变用户当前的 `cli_auth_credentials_store`。
- Codex `file` 槽使用字段合并、同目录原子替换、读回验证与直接回滚。
- Codex `keyring` / `auto` 不检查 CLI 版本号。YAAT 查询实际配置的 Codex CLI 所报告的 `secret_auth_storage` feature，再选择直接系统钥匙串后端，或“系统钥匙串中的口令 + Age 加密 secrets 文件”后端；无法确认布局时失败关闭。
- Codex 凭据切换只替换 `auth_mode`、`OPENAI_API_KEY`、`tokens`、`last_refresh` 四个账号字段；`agent_identity`、个人访问令牌、Bedrock 凭据以及未来未知字段都从当前凭据槽保留。
- Codex `auto` 读取时遵循“首选 Keyring、缺项时读取 `auth.json`”；切换时写入并读回验证首选 Keyring，再清理旧文件回退。Keyring 检查报错时不会假装缺项后降级到文件。
- Codex `ephemeral` 无法跨进程复制，仍会被拒绝。
- Claude Code 的全局私有凭据切换不检查 CLI 版本号。macOS 按 Claude Code 的派生规则使用 Keychain，并在主条目明确缺失时识别 `.credentials.json` 回退；Linux 使用 `.credentials.json`；Windows 只有在官方 feature / 强制开关启用时才使用 Credential Manager，否则使用文件。
- Claude Desktop 在 macOS 上使用 `Claude Safe Storage / Claude Key` 派生 Electron `v10` 密钥；Windows 从 `Local State` 读取由当前用户 DPAPI 包装的密钥，再按 Electron `v10` 的 AES-256-GCM 格式编解码。Cookie 数据库按 Chromium v24 的域名哈希规则处理。检测到未适配但仍可验证读取的 Cookie 数据库版本时，操作结果会通过统一 warning 通道在右下角提示一次；涉及写入或无法确认格式兼容时会停止操作，不会按旧格式覆盖。切换前会保存全局使用期间刷新的最新账号快照，避免下次切回过期凭据。
- 全局基线明确区分“尚未接管官方凭据”“接管前有账号”和“接管前凭据为空”。空槽可以直接写入首个账号；用户主动 logout 后仍可停止全局管理，并只清除 YAAT 接管的账号字段。
- Windows Credential Manager 同时支持 Bun 单条目与 `#m` / `#p` / `#0..` 分块布局。分块写入使用 pending 标记、逐块发布、元数据提交和完整读回验证；任一步失败都会返回具体阶段并尽力恢复原值，回滚失败也会明确显示且保留未完成标记，不会把超过 2400 字节的文档伪装成单条目。
- 写入后读回验证账号字段；验证失败时直接尝试恢复切换前的凭据并返回错误。

## 本地用量统计

YAAT 只读取 Codex / Claude Code 会话文件中的时间、请求标识与 Token 用量元数据，不把提示词、回复正文或工具内容写入数据库。

- Codex：索引 `sessions` 与 `archived_sessions`，处理累计计数、重置、归档移动和不完整文件尾部。
- Claude Code：索引 `projects` 下的 JSONL，会处理主会话、子代理、重放、初步 / 最终批次和缓存 Token。
- 每次扫描重建所选平台的本地用量快照；稳定事件键和最终记录优先规则避免重复，并允许最终批次替换初步批次。Codex 同时支持普通 `.jsonl` 和 `.jsonl.zst`；单个源文件超过 512 MiB 时跳过并标记为部分统计。
- 日期查询最长 366 天，按用户选择的 IANA 时区聚合。

## 会话历史统一

会话历史入口位于设置中，所有写操作都要求对应客户端已经退出。预览用于展示计划，应用时会重新扫描当前文件。

- Codex：YAAT 生成的官方与第三方配置都使用稳定的 `yaat_managed_v1` 会话桶。现有 `sessions` / `archived_sessions` 中的 `.jsonl` 与 `.jsonl.zst` 会同时修正 `session_meta.payload.model_provider` 和最新版 Codex 的 `thread_settings_applied.thread_settings.model_provider_id`。全局目录和各托管 `CODEX_HOME` 之间会同步缺失会话以及已有会话的单向追加记录；只有双方都产生不同后续内容时才报告冲突。Codex 会根据 JSONL 自行修复 `state_5.sqlite`，YAAT 不修改官方状态数据库。
- Claude Code：在全局配置目录和各托管 `CLAUDE_CONFIG_DIR/projects` 之间同步缺失的 JSONL 会话和单向追加记录；真正分叉的会话保留冲突，凭据、设置、MCP、插件、缓存与遥测目录不会进入同步范围。
- Claude Desktop：扫描普通 `Claude`、企业版 `Claude-3p` 和所有 YAAT 隔离 Desktop Profile 下的 `claude-code-sessions/<账号 UUID>/<组织 UUID>/local_*.json`。用户必须明确选择目标账号 / 组织；缺失会话会复制到目标，已有会话只有在某一份是其他版本的严格结构化扩展时才用最长版本更新目标，真正分叉则报告冲突且不覆盖。来源始终保持不变。
- 设置中的“切换时同步”默认关闭。隔离启动会先尝试同步再打开终端 / Desktop，全局切换在成功后同步；同步失败不会撤销已完成的账号操作，而会显示可重试警告。Claude Desktop 必须先保存明确的目标账号 / 组织。
- Claude Desktop 的 Chat / Cowork 不在这一功能内。它们位于 `local-agent-mode-sessions`，包含上传、输出、记忆以及由系统钥匙串密钥保护的 HMAC 审计链，不能按 Code 会话文件处理。

## 开发

完整的安全开发流程、代码约定、生成文件边界和提交前检查见 [CONTRIBUTING.md](CONTRIBUTING.md)。

要求：

- Rust `1.97.1`（由 `rust-toolchain.toml` 固定）
- Node.js 22.x 与 pnpm 11.18.0（由 `package.json` 固定）
- Tauri CLI 2.x（作为项目开发依赖安装）
- macOS：Xcode Command Line Tools
- Windows：Visual Studio C++ Build Tools 与 WebView2

常用命令：

```bash
# 安装前端依赖
pnpm install --frozen-lockfile

# React/Vite 浏览器预览（使用本地预览数据，不连接 Tauri 后端）
VITE_YAAT_PREVIEW=1 pnpm run dev

# Tauri 开发模式（仅用于不含真实客户端数据的隔离测试用户或虚拟机）
pnpm run tauri dev

# 宿主机静态门禁：不启动应用、不运行测试
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
pnpm run format:check

# 前端类型检查
pnpm run typecheck

# 前端生产构建
pnpm run build

# 完整验证在 Docker 中执行 Rust 文档、测试和 Tauri 无安装包构建
docker build --file docker/app-build.Dockerfile --tag yaat-app-build .

# 在 Docker 中交叉检查 Windows 专有代码与全部 Rust targets
docker build --file docker/windows-check.Dockerfile --tag yaat-windows-check .

# 完全在 Docker 中执行前端严格类型检查和生产构建
docker build --file docker/ui.Dockerfile --tag yaat-ui-check:react .

# 在完全隔离、运行阶段断网的 Docker 容器中安装真实 Codex，并验证
# YAAT 的严格配置、动态凭据助手和官方账号私有凭据复制切换
sh scripts/test-codex-docker.sh

# 安装真实 Claude Code，验证生产适配器生成的设置与隔离启动环境
sh scripts/test-claude-docker.sh

# 本机正式打包需要 updater 签名私钥
TAURI_SIGNING_PRIVATE_KEY_PATH=/path/to/yaat-updater.key pnpm run tauri build
```

GitHub Actions 会在每次推送和 Pull Request 上执行前端门禁及 Linux、macOS、Windows Rust 门禁。仓库的 `Release` 工作流由维护者手动启动，按 `package.json` / Cargo workspace / `tauri.conf.json` 中的应用版本创建草稿 Release，构建 macOS Apple Silicon、macOS Intel 与 Windows x64 安装包，并生成 Tauri updater 所需的签名和 `latest.json`。草稿发布后才会被已安装的 YAAT 检测到。

Codex 互操作镜像固定安装官方 `@openai/codex 0.146.0`。测试源码不挂载宿主机账号目录；容器运行阶段使用 `--network none`、移除全部 capabilities，并只与容器内的 mock Responses 服务通信。测试还会用两组完全虚构的 ChatGPT `auth.json` 快照执行 B → A 复制切换，要求 Codex 对两次结果都报告已登录，同时确认用户自有 Provider、MCP 配置、注释和非账号凭据字段未丢失。

Claude 互操作镜像固定安装官方 `@anthropic-ai/claude-code 2.1.220`，直接复用生产 Claude 适配器生成全局与隔离 `settings.json`，验证注释、permissions、MCP 和未知字段保留，并让真实 CLI 在断网容器中加载同一隔离环境。这些固定版本是可重复的互操作测试基准，不是运行时版本门禁；升级测试基准时仍需重新审查官方凭据布局并重跑测试。

## 目录

```text
crates/yaat-contracts/   前后端共享 IPC 数据契约
src/                     React / TypeScript 前端与 shadcn 风格组件
src-tauri/src/           Tauri 后端、数据库、平台互操作、适配器与统计
src-tauri/src/activation 字段级补丁、原子写入与字段级回滚
src-tauri/src/platform/  Codex / Claude Code / Claude Desktop 平台适配器
src-tauri/src/usage/     Codex / Claude Code 本地日志解析与索引服务
docs/architecture.md     安全边界与切换事务说明
CONTRIBUTING.md          安全开发流程、代码约定与完整门禁
docker/                  前端/应用构建、视觉回归与真实客户端互操作镜像
tools/codex-interop/     直接复用生产适配器源码的轻量测试工具
tools/claude-interop/    真实 Claude Code 配置与启动环境互操作工具
```

## 许可

项目使用 MIT License，见 [LICENSE](LICENSE)。
