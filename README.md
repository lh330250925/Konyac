# konyac

基于 Rust 的 AI 翻译工具，采用 Client / Daemon 架构，支持终端快捷交互与多协议后端。

## 特性

- **多协议原生适配**：统一抽象并原生支持 OpenAI（兼容各类本地与云端服务）、Anthropic Messages、Google Gemini 协议。
- **IPC / 双通道故障回退**：服务端支持同时监听 Unix Domain Socket 与 TCP，客户端优先通过 Unix Socket 零开销直连，异常时自动平滑回退至 TCP。
- **双模翻译适配引擎**：针对单词/短语自动激活 Markdown 词典模式（音标、释义、典型例句）；针对长句长文进行直接意译并严格保护代码及符号排版。
- **系统剪贴板集成**：原生集成系统剪贴板（兼容 Wayland 与 X11），支持快速读取翻译（`-p`）与结果自动回写（`-C`）。

## 安装与快速开始

### 安装

```bash
cargo install --path .
```

### 启动后台服务

```bash
# 直接前台启动
konyac serve

# 或通过 systemd (User Service) 后台常驻 (推荐)
systemctl --user enable --now konyac
```
> 首次运行会自动在 `~/.config/konyac/config.toml` 生成默认配置。

#### 配置 systemd 用户服务 (`~/.config/systemd/user/konyac.service`)

```ini
[Unit]
Description=konyac AI Translation Daemon
After=network.target

[Service]
ExecStart=%h/.local/bin/konyac serve
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
```

### 命令行使用

```bash
# 单词查询（词典模式）
konyac ephemeral

# 句子翻译
konyac "Concurrency is not parallelism."

# 管道输入 (stdin)
git log -1 --pretty=%B | konyac

# 指定目标语言（默认 zh-CN）
konyac -t ja "Hello world"

# 指定服务与模型
konyac -s ollama -m qwen2.5:7b "Translate this"

# 附带补充说明或上下文
konyac -e "保留专业名词" "Zero-cost abstractions"

# 从系统剪贴板读取并翻译 (paste)
konyac -p

# 翻译并自动将结果写入剪贴板 (copy)
konyac -C "Zero-cost abstractions"

# 快捷流：翻译剪贴板内容并写回剪贴板
konyac -p -C
```

## CLI 参考

```text
konyac [OPTIONS] [TEXT]...
konyac <SUBCOMMAND>
```

### 选项

| 参数 | 环境变量 | 说明 |
| :--- | :--- | :--- |
| `[TEXT]...` | - | 待翻译文本（为空时从 stdin 读取） |
| `-t, --target <LANG>` | - | 目标语言（默认 `zh-CN`） |
| `-s, --service <NAME>` | - | 指定 LLM 服务 |
| `-m, --model <MODEL>` | - | 指定模型 |
| `-e, --extra <TEXT>` | - | 额外上下文或翻译指令 |
| `-p, --paste` | - | 从系统剪贴板读取待翻译文本 |
| `-C, --copy` | - | 将翻译结果自动复制到系统剪贴板 |
| `--no-spinner` | - | 禁用首字等待动画（默认开启） |
| `-c, --config <FILE>` | `KONYAC_CONFIG` | 指定配置文件路径 |
| `--endpoint <URL>` | `KONYAC_ENDPOINT` | 指定 Daemon 地址（支持 `http://...` 或 `unix:/path/to.sock`） |

### 子命令

- `konyac serve [-p <PORT>] [--host <HOST>] [-u <SOCKET>]`：启动守护进程
- `konyac health`：检查服务状态
- `konyac services`：查看已配置的服务和模型
- `konyac completions <bash|zsh|fish|powershell|elvish>`：生成 Shell 自动补全脚本

#### 安装 Shell 自动补全

```bash
# Zsh (添加到 fpath 目录，例如 ~/.zfunc)
konyac completions zsh > ~/.zfunc/_konyac

# Bash
konyac completions bash > ~/.local/share/bash-completion/completions/konyac

# Fish
konyac completions fish > ~/.config/fish/completions/konyac.fish
```

## 配置示例 (`~/.config/konyac/config.toml`)

```toml
[server]
# Unix domain socket 路径（Linux 默认推荐使用 $XDG_RUNTIME_DIR/konyac.sock）
unix = "/run/user/1000/konyac.sock"

[server.tcp]
host = "127.0.0.1"
port = 16742

[default]
service = "deepseek"
model = "deepseek-chat"

# 本地模型 (Ollama / vLLM / LMStudio，无需 API Key)
[[service]]
name = "ollama"
protocol = "openai"
base_url = "http://localhost:11434/v1"
models = ["qwen2.5:7b"]

# 云端 API（支持 ${ENV_VAR} 读取环境变量）
[[service]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "${DEEPSEEK_API_KEY}"
models = ["deepseek-chat"]

[prompt]
target_lang = "zh-CN"
```

## API 接口文档

Daemon 提供标准的 HTTP 与 SSE 接口，便于集成到编辑器插件（Neovim、VS Code、Raycast）、外部脚本或自动化工作流中。

### 接口概览

| 方法 | 路径 | 响应类型 | 说明 |
| :--- | :--- | :--- | :--- |
| `GET` | `/health` | `application/json` | 服务健康检查与活动服务计数 |
| `GET` | `/services` | `application/json` | 获取所有已加载的 LLM 服务及模型列表 |
| `POST` | `/translate` | `text/event-stream` | 流式发起翻译请求（SSE） |

---

### `POST /translate` 发起流式翻译

#### 请求头 (Headers)
```http
Content-Type: application/json
Accept: text/event-stream
```

#### 请求参数 (Request Body)

| 字段 | 类型 | 必填 | 说明 |
| :--- | :--- | :---: | :--- |
| `text` | `string` | 是 | 待翻译的原始文本，不能为空 |
| `target_lang` | `string` | 是 | 目标语言标识（如 `zh-CN`, `en`, `ja`, `de` 等） |
| `service` | `string` | 是 | 指定后端 LLM 服务标识（对应配置文件中 `[[service]].name`） |
| `model` | `string` | 是 | 调用的模型名称（必须属于该服务已配置的 `models` 列表） |
| `prompt` | `object` | 否 | 自定义提示词配置（见下表） |
| `prompt.system` | `string` | 否 | 覆盖全局系统提示词（必须保留 `{target_lang}` 占位符） |
| `prompt.extra` | `string` | 否 | 客户端附加指令或术语背景（追加至系统提示词后） |
| `history` | `array` | 否 | 历史上下文消息列表，用于多轮对话或连贯翻译 |
| `history[].role` | `string` | 是 | 消息角色（`user` 或 `assistant`） |
| `history[].content`| `string` | 是 | 消息正文 |
| `json_schema` | `object` | 否 | 结构化输出规范，底层驱动会自动传递给上游模型 |

#### 响应格式 (Response)

- **`200 OK` (SSE 流式输出)**：
  - 成功时返回 `Content-Type: text/event-stream`。
  - 数据块以标准 SSE `data:` 逐字推送：
    ```text
    data: 你好
    data: ，
    data: 世界
    ```
  - 若在流式传输过程中上游抛出异常，会下发命名错误事件：
    ```text
    event: error
    data: Rate limit exceeded
    ```
  - 内置保活机制（Keep-Alive），防止网络中间代理超时断开。
- **`400 Bad Request`**：请求参数缺失或非法（如必填字段为空、模型未在该服务中配置），返回：
  ```json
  { "error": "Field 'text' cannot be empty" }
  ```
- **`502 Bad Gateway`**：上游 LLM 服务连接失败或返回错误，返回：
  ```json
  { "error": "Upstream error: Connection refused" }
  ```

#### 请求示例

##### 1. 基础翻译请求
```bash
# TCP 访问
curl -N -X POST http://127.0.0.1:16742/translate \
  -H "Content-Type: application/json" \
  -d '{
    "text": "Hello world",
    "target_lang": "zh-CN",
    "service": "deepseek",
    "model": "deepseek-chat"
  }'

# Unix Domain Socket 访问 (零网络栈开销)
curl -N -X POST --unix-socket /run/user/1000/konyac.sock http://localhost/translate \
  -H "Content-Type: application/json" \
  -d '{
    "text": "Hello world",
    "target_lang": "zh-CN",
    "service": "deepseek",
    "model": "deepseek-chat"
  }'
```

##### 2. 携带业务上下文与翻译指令 (`prompt.extra`)
```bash
curl -N -X POST http://127.0.0.1:16742/translate \
  -H "Content-Type: application/json" \
  -d '{
    "text": "The runtime manages cooperative scheduling.",
    "target_lang": "zh-CN",
    "service": "deepseek",
    "model": "deepseek-chat",
    "prompt": {
      "extra": "专有名词保留英文，语境为操作系统内核开发"
    }
  }'
```

##### 3. 携带多轮对话历史 (`history`)
```bash
curl -N -X POST http://127.0.0.1:16742/translate \
  -H "Content-Type: application/json" \
  -d '{
    "text": "What about its memory safety?",
    "target_lang": "zh-CN",
    "service": "deepseek",
    "model": "deepseek-chat",
    "history": [
      { "role": "user", "content": "Rust provides fearless concurrency." },
      { "role": "assistant", "content": "Rust 提供了无畏并发。" }
    ]
  }'
```

---

### 辅助管理接口

#### 1. `GET /health`
```bash
# TCP
curl -s http://127.0.0.1:16742/health

# Unix Domain Socket
curl -s --unix-socket /run/user/1000/konyac.sock http://localhost/health
```
```json
{
  "status": "ok",
  "version": "0.1.0",
  "services_count": 2
}
```

#### 2. `GET /services`
```bash
# TCP
curl -s http://127.0.0.1:16742/services

# Unix Domain Socket
curl -s --unix-socket /run/user/1000/konyac.sock http://localhost/services
```
```json
[
  {
    "name": "deepseek",
    "protocol": "openai",
    "configured_models": ["deepseek-chat"]
  },
  {
    "name": "ollama",
    "protocol": "openai",
    "configured_models": ["qwen2.5:7b"]
  }
]
```
