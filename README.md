# konyac

[![CI](https://github.com/lhxn/Konyac/actions/workflows/ci.yml/badge.svg)](https://github.com/lhxn/Konyac/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/lhxn/Konyac)](https://github.com/lhxn/Konyac/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey)

基于 Rust 的轻量级 AI 翻译守护进程与 CLI（Linux / macOS），采用 Client / Daemon 架构。

- **多协议支持**：原生适配 OpenAI Responses、Anthropic Messages、Google Gemini Interactions，统一支持 `max_tokens`、`thinking`、`prompt_cache`、`json_schema`、自定义 `headers` 与多模态图片翻译。
- **双栈监听**：支持 Unix Domain Socket、TCP 或同时监听（客户端优先走 Unix Socket 并自动回退 TCP）。
- **流式与容错**：SSE 流式输出、HTTP/2 空闲保活、首字节前瞬断与 `429`/`502`/`503`/`504` 自动退避重试。

## 安装

### Homebrew（macOS / Linux）

```bash
brew install lhxn/tap/konyac
brew services start konyac
```

### Arch Linux

```bash
git clone https://github.com/lhxn/Konyac.git
cd Konyac/packaging/aur && makepkg -si
systemctl --user enable --now konyac
```

### 预编译二进制 / Cargo 源码构建

可从 [Releases](https://github.com/lhxn/Konyac/releases/latest) 下载预编译包，或直接从源码安装：

```bash
cargo install --path . --root "$HOME/.local"
```

## 快速开始

```bash
mkdir -p ~/.config/konyac
cp config.example.toml ~/.config/konyac/config.toml

# 启动守护进程（或 systemctl --user enable --now konyac）
konyac serve
```

```bash
konyac ephemeral                                 # 单词查询（词典模式）
konyac "Concurrency is not parallelism."         # 句子翻译
git log -1 --pretty=%B | konyac                  # 管道输入
konyac -t ja "Hello world"                       # 指定目标语言
konyac -s ollama -m qwen2.5:7b "Translate this"  # 指定服务与模型
konyac -e "保留专业名词" "Zero-cost abstractions" # 附加指令
konyac --thinking low "Hello world"              # 调整推理深度
konyac -i screenshot.png                         # 图片翻译
```

## CLI 参数

| 参数 | 环境变量 | 说明 |
| :--- | :--- | :--- |
| `[TEXT]...` | - | 待翻译文本（留空时从 stdin 读取；传 `-i` 时可选） |
| `-t, --target <LANG>` | - | 覆盖默认目标语言 |
| `-s, --service <NAME>` | - | 覆盖默认 LLM 服务 |
| `-m, --model <MODEL>` | - | 覆盖默认模型 |
| `--thinking <LEVEL>` | - | 推理深度：`minimal`、`low`、`medium`、`high` |
| `-i, --image <FILE>` | - | 图片翻译（PNG/JPEG/WebP/GIF，可多次指定） |
| `-e, --extra <TEXT>` | - | 额外上下文或指令 |
| `-c, --config <FILE>` | `KONYAC_CONFIG` | 配置文件路径 |
| `--endpoint <URL>` | `KONYAC_ENDPOINT` | 指定 Daemon 地址（`http://...`、`https://...` 或 `unix:/path/to.sock`） |

子命令：
- `konyac serve [--tcp <ADDR>] [-u <SOCKET>]`：启动守护进程
- `konyac health`：查看服务状态、默认语言及已加载的服务与模型

## 配置 (`~/.config/konyac/config.toml`)

首个 `[[service]]` 及其首个模型为默认项；`api_key` 可选，支持 `${ENV_VAR}` 注入环境变量。完整模板见 [`config.example.toml`](config.example.toml)。

```toml
# 默认同时启用自适应 Unix Socket（Linux: $XDG_RUNTIME_DIR/konyac.sock，macOS: $TMPDIR/konyac-<uid>/konyac.sock）
# 与 TCP (127.0.0.1:16742)；如需单栈监听或改端口，取消注释 [server] 并只保留对应项：
# [server]
# unix = "/run/user/1000/konyac.sock"
# tcp = "127.0.0.1:16742"

[[service]]
name = "ollama"
protocol = "openai"
base_url = "http://localhost:11434/v1"
models = ["qwen2.5:7b"]

[[service]]
name = "deepseek"
protocol = "openai"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
models = ["deepseek-flash"]
# max_tokens = 8192
# thinking = "low"
# prompt_cache = true
# headers = { "HTTP-Referer" = "https://example.com" }

[prompt]
target_lang = "zh-CN"
```

| `protocol` | 上游接口 | 官方 `base_url` |
| :--- | :--- | :--- |
| `openai` | `POST /v1/responses` | `https://api.openai.com/v1` |
| `anthropic` | `POST /v1/messages` (`2026-10-21`) | `https://api.anthropic.com/v1` |
| `gemini` | `POST /v1/interactions` | `https://generativelanguage.googleapis.com` |

- **Unix Socket**：默认位于 `$XDG_RUNTIME_DIR/konyac.sock`，通过文件权限控制访问。
- **TCP 鉴权**：设置 `KONYAC_TCP_TOKEN` 后所有 TCP 请求均需携带 `Authorization: Bearer <token>`（CLI 会自动读取同名环境变量）；监听非回环地址时必须设置该变量。

## HTTP API

### `GET /health`

```json
{
  "status": "ok",
  "version": "0.1.1",
  "target_lang": "zh-CN",
  "services": [
    {
      "name": "deepseek",
      "protocol": "openai",
      "models": ["deepseek-flash"],
      "thinking": "low"
    }
  ]
}
```

### `POST /translate` (`text/event-stream`)

| 字段 | 类型 | 必填 | 说明 |
| :--- | :--- | :---: | :--- |
| `text` | `string` | 否 | 待翻译文本（与 `images` 不能同时为空） |
| `images` | `array` | 否 | 最多 4 张图片（总 base64 ≤ 12 MiB），格式：`{"mime_type":"image/png","data":"<base64>"}` |
| `target_lang` | `string` | 否 | 目标语言（默认取 `[prompt].target_lang`） |
| `service` | `string` | 否 | 服务名（默认取首个 `[[service]]`） |
| `model` | `string` | 否 | 模型名（须属于所选服务） |
| `thinking` | `string` | 否 | `minimal`、`low`、`medium`、`high` |
| `system` | `string` | 否 | 覆盖系统提示词（支持 `{target_lang}` 占位符） |
| `extra` | `string` | 否 | 追加指令或术语说明 |
| `history` | `array` | 否 | 多轮上下文：`[{"role":"user\|assistant","content":"..."}]` |
| `json_schema` | `object` | 否 | 结构化输出 JSON Schema |
| `cache_key` | `string` | 否 | 自定义 OpenAI `prompt_cache_key` |

#### SSE 事件与错误码

- 文本块：`data: <chunk>`
- 正常结束：`event: done` + `data: [DONE]`
- 流内错误：`event: error` + `data: {"code":"...","error":"..."}`（与非 200 HTTP 错误响应体结构一致）

| 错误码 | 说明 |
| :--- | :--- |
| `invalid_request` | 参数缺失、类型错误、空输入或图片格式非法 |
| `service_not_configured` / `model_not_configured` | 服务或模型未配置 |
| `unauthorized` / `forbidden` | TCP Token 或 Host 校验被拒绝 |
| `upstream_timeout` / `upstream_interrupted` | 上游超时或流中途断开 |
| `rate_limited` | 上游触发频率或配额限制 |
| `output_truncated` | 达到最大输出 token 上限 |
| `content_blocked` | 上游安全策略拦截 |
| `upstream_error` | 其他上游错误 |

```bash
# TCP
curl -N -X POST http://127.0.0.1:16742/translate \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello world"}'

# Unix Socket
curl -N -X POST --unix-socket /run/user/1000/konyac.sock http://localhost/translate \
  -H "Content-Type: application/json" \
  -d '{"text":"Hello world","target_lang":"ja"}'
```
