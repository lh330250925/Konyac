# konyac

[![CI](https://github.com/lh330250925/Konyac/actions/workflows/ci.yml/badge.svg)](https://github.com/lh330250925/Konyac/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/lh330250925/Konyac)](https://github.com/lh330250925/Konyac/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey)

基于 Rust 的轻量级 AI 翻译守护进程与命令行工具，面向 Linux 与 macOS，采用 Client / Daemon 架构，支持多协议上游与灵活监听模式。

## 特性

- **多协议原生适配**：支持 OpenAI Responses、Anthropic Messages、Google Gemini Interactions 及兼容服务；原生统一映射输出上限（`max_tokens`）、推理深度（`thinking`）、前缀缓存（`prompt_cache`）、结构化输出（`json_schema`）与多模态图片翻译。
- **高稳定代理层**：启用 HTTP/2 长连接空闲保活与首字节前瞬断/`429`/`502`/`503`/`504` 自动退避重试，支持按服务配置自定义加密敏感请求头（`headers`）。
- **三种监听模式与自动回退**：服务端支持**仅 Unix Domain Socket**、**仅 TCP** 或**同时监听两者**；同时监听时客户端优先连接 Unix Socket，失败时自动回退至 TCP。
- **简洁防注入翻译提示词**：针对独立单词/术语输出 Markdown 词条，针对长句长文直接意译并保护代码、标签与占位符排版。

## 安装与快速开始

### 安装

```bash
cargo install --path . --root "$HOME/.local"
```

### 准备配置与启动后台服务

```bash
# 复制示例配置并填入至少一个 [[service]]
mkdir -p ~/.config/konyac
cp config.example.toml ~/.config/konyac/config.toml

# 直接前台启动
konyac serve

# 或通过 systemd (User Service) 后台常驻 (Linux 推荐)
systemctl --user enable --now konyac
```

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

# 调整推理深度（如极速查词用 minimal / low，长文精翻用 high）
konyac --thinking low "Hello world"

# 截屏 / 图片多模态翻译（可不传文本，直接翻译图片中的文字）
konyac -i screenshot.png
```

## CLI 参考

```text
konyac [OPTIONS] [TEXT]...
konyac <SUBCOMMAND>
```

### 选项

| 参数 | 环境变量 | 说明 |
| :--- | :--- | :--- |
| `[TEXT]...` | - | 待翻译文本（为空时从 stdin 读取；若指定 `--image` 则文本可选） |
| `-t, --target <LANG>` | - | 覆盖 Daemon 的默认目标语言 |
| `-s, --service <NAME>` | - | 覆盖 Daemon 的默认 LLM 服务 |
| `-m, --model <MODEL>` | - | 覆盖所选服务的默认模型 |
| `--thinking <LEVEL>` | - | 覆盖服务的默认推理深度（`minimal`、`low`、`medium`、`high`） |
| `-i, --image <FILE>` | - | 传入图片文件进行多模态翻译（支持 PNG/JPEG/WebP/GIF，可多次指定） |
| `-e, --extra <TEXT>` | - | 额外上下文或翻译指令 |
| `-c, --config <FILE>` | `KONYAC_CONFIG` | 指定 Daemon 的配置文件。CLI 可从中读取 `[server]` 来定位 Daemon |
| `--endpoint <URL>` | `KONYAC_ENDPOINT` | 指定 Daemon 地址（支持 `http://...`、`https://...` 或 `unix:/path/to.sock`） |

指定 `--endpoint` 或 `KONYAC_ENDPOINT` 后，CLI 可以直接连接，无需本地配置文件。

### 子命令

- `konyac serve [--tcp <ADDR>] [-u <SOCKET>]`：启动守护进程
- `konyac health`：查看服务状态、默认目标语言以及已配置的服务和模型

## 配置示例 (`~/.config/konyac/config.toml`)

第一个 `[[service]]` 及其 `models` 列表中的首个模型即为默认服务与默认模型；`api_key` 始终可选（便于连接本机、局域网或内网容器服务）。

```toml
# 监听模式：仅写 unix 即为仅 Unix；仅写 tcp 即为仅 TCP；两者都写则同时监听
[server]
unix = "/run/user/1000/konyac.sock"
tcp = "127.0.0.1:16742"

# 本地或局域网模型（Ollama 0.13.3 或更高版本，无需 API Key）
[[service]]
name = "ollama"
protocol = "openai"
base_url = "http://localhost:11434/v1"
models = ["qwen2.5:7b"]

# 云端 API（支持 ${ENV_VAR} 读取环境变量）
[[service]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
models = ["deepseek-flash"]
# 可选：统一输出 token 上限（Anthropic 默认 8192，OpenAI/Gemini 未配置时不限）
# max_tokens = 8192
# 可选：默认推理/思考深度（minimal / low / medium / high）
# thinking = "low"
# 可选：启用前缀缓存（OpenAI 自动生成 prompt_cache_key；Anthropic 添加 ephemeral cache_control）
# prompt_cache = true
# 可选：自定义上游请求头（值同样支持 ${ENV_VAR}，内部标记为敏感信息）
# headers = { "HTTP-Referer" = "https://example.com" }

[prompt]
target_lang = "zh-CN"
```

| `protocol` | 上游接口 | 官方 `base_url` 示例 |
| :--- | :--- | :--- |
| `openai` | OpenAI Responses `POST /v1/responses` | `https://api.openai.com/v1` |
| `anthropic` | Anthropic Messages `POST /v1/messages`，版本 `2026-10-21` | `https://api.anthropic.com/v1` |
| `gemini` | Gemini Interactions `POST /v1/interactions` | `https://generativelanguage.googleapis.com` |

### TCP 访问

默认的回环 TCP 接口接受 `localhost` 或回环 IP 作为 HTTP Host。配置 `KONYAC_TCP_TOKEN` 后，所有 TCP 请求都须携带 `Authorization: Bearer <token>`，CLI 会从同名环境变量读取它。监听非回环地址时，Daemon 要求设置该变量。通过网络访问时，可在 Daemon 前设置 HTTPS 反向代理，并用 `--endpoint https://...` 连接。

Unix Socket 使用文件权限控制访问。默认路径使用 `$XDG_RUNTIME_DIR/konyac.sock`，没有运行时目录时使用临时目录下的每用户子目录。TCP 接口默认关闭浏览器跨域访问。

## API 接口文档

Daemon 提供标准的 HTTP 与 SSE 接口，便于集成到编辑器插件（Neovim、VS Code、Raycast）、外部脚本或自动化工作流中。

### 接口概览

| 方法 | 路径 | 响应类型 | 说明 |
| :--- | :--- | :--- | :--- |
| `GET` | `/health` | `application/json` | 服务健康检查、默认目标语言及按配置顺序返回的服务和模型列表（首项为默认服务） |
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
| `text` | `string` | 否 | 待翻译文本；若提供 `images` 时可省略或留空（两者不能同时为空） |
| `images` | `array` | 否 | 图片列表（最多 4 张，总 base64 ≤ 12 MiB），每项为 `{"mime_type":"image/png","data":"<raw base64>"}`（支持 PNG/JPEG/WebP/GIF） |
| `target_lang` | `string` | 否 | 默认使用 Daemon 的 `[prompt].target_lang`（内置值为 `zh-CN`） |
| `service` | `string` | 否 | 默认使用第一个配置的 `[[service]]` |
| `model` | `string` | 否 | 默认使用所选服务 `models` 列表中的首个模型。显式指定的模型须属于所选服务 |
| `thinking` | `string` | 否 | 覆盖服务默认推理深度：`minimal`、`low`、`medium`、`high` |
| `system` | `string` | 否 | 覆盖系统提示词；若包含 `{target_lang}` 会自动替换，未包含也可用于自定义或 `json_schema` 任务 |
| `extra` | `string` | 否 | 客户端附加指令或术语背景（追加至系统提示词后） |
| `history` | `array` | 否 | 历史上下文消息列表，用于多轮对话或连贯翻译 |
| `history[].role` | `string` | 是 | `user` 或 `assistant`（也接受 `model` 作为 `assistant` 的别名）。忽略大小写和首尾空白，其他角色返回 HTTP `400` |
| `history[].content`| `string` | 是 | 消息正文 |
| `json_schema` | `object` | 否 | 结构化输出规范，由上游按各自支持的规则处理。客户端可在收到完整流后校验结果 |
| `cache_key` | `string` | 否 | OpenAI Responses API 的 `prompt_cache_key` 路由提示（覆盖默认生成 key，便于多轮/术语表会话复用缓存） |

三协议特性映射：`json_schema` 对应 OpenAI `text.format`、Anthropic `output_config.format`、Gemini `response_format`；`thinking` 对应 OpenAI `reasoning.effort`、Anthropic `output_config.effort`、Gemini `generation_config.thinking_level`；`images` 对应 OpenAI `input_image`、Anthropic `image`（base64 source）、Gemini `image` 内容块。

#### 响应格式 (Response)

- **`200 OK` (SSE 流式输出)**：
  - 成功时返回 `Content-Type: text/event-stream`。
  - 数据块以标准 SSE `data:` 逐字推送：
    ```text
    data: 你好
    data: ，
    data: 世界
    ```
  - 翻译完成时发送 `event: done`，其数据为 `[DONE]`。
  - 如果上游报错、提前断流或达到输出限制，会发送 `event: error`。其数据是与 HTTP 错误相同的 JSON 对象：
    ```text
    event: error
    data: {"code":"output_truncated","error":"Responses API output incomplete: max_output_tokens"}
    ```
  - 内置保活机制（Keep-Alive），防止网络中间代理超时断开。
- **`400 Bad Request`**：`text` 与 `images` 同时为空、图片格式或 base64 非法、字段为空、服务未知或模型未在所选服务中配置。
- **`422 Unprocessable Entity`**：JSON 字段类型不符合要求或无法解析。
- **`502 Bad Gateway`**：上游 LLM 服务连接失败或返回错误。

| 错误码 | 含义 |
| :--- | :--- |
| `invalid_request` | 请求体缺失字段、类型错误或包含空值等非法参数 |
| `service_not_configured` / `model_not_configured` | 服务或模型未配置 |
| `unauthorized` / `forbidden` | TCP 鉴权或访问限制拒绝了请求 |
| `upstream_timeout` | 等待模型服务连接、响应或继续输出时超时 |
| `rate_limited` | 模型服务触发频率或额度限制 |
| `output_truncated` | 达到模型输出或上下文上限。流式译文可标记为未完成 |
| `content_blocked` | 模型拒绝请求或阻止输出 |
| `upstream_interrupted` | 模型流在完成前断开 |
| `upstream_error` | 其他模型服务错误 |

#### 请求示例

```bash
# TCP 访问，使用 Daemon 的默认值
curl -N -X POST http://127.0.0.1:16742/translate \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello world"}'

# Unix Domain Socket 访问，只覆盖目标语言
curl -N -X POST --unix-socket /run/user/1000/konyac.sock http://localhost/translate \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello world","target_lang":"ja"}'
```

### 状态与服务查询 `GET /health`

```json
{
  "status": "ok",
  "version": "0.1.0",
  "target_lang": "zh-CN",
  "services": [
    {
      "name": "deepseek",
      "protocol": "openai",
      "models": ["deepseek-flash"],
      "thinking": "low"
    },
    {
      "name": "ollama",
      "protocol": "openai",
      "models": ["qwen2.5:7b"]
    }
  ]
}
```
