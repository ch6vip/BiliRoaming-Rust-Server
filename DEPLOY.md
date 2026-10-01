# 部署与验证报告

> 基线 commit：`5ae4646`（= upstream/main，最后更新 2023-09-18）
> 验证时间：2026-10 ｜ 环境：Windows + Redis + sing-box 代理
> 本文档合并了部署指南与实测验证结论，是当前唯一的权威参考。

---

## 摘要（TL;DR）

| 问题 | 结论 |
|---|---|
| 项目还能用吗？ | **能用**。核心功能（区域解锁）实测有效 |
| 老签名还有效吗？ | **有效**。`appkey + appsec + MD5` 仍被 B 站接受 |
| 需要大会员吗？ | **不需要**。免费锁区番剧已验证可解锁，服务无会员硬门槛 |
| 主要风险 | 部分上游接口失效及 HTTP/TLS 等历史依赖安全告警，见第 13 节 |
| 本仓库已做的修复 | 原有兼容修复及第 13 节的请求边界、凭据、缓存、后台并发和 CI 修复 |

**验证到什么程度**：经服务 → 台湾出口 → B 站，成功取流并**实际下载**（HTTP 206，`ftypiso5` 合法 MP4）。

---

# 第一部分：部署

## 1. 快速开始

```bash
# 1. 准备配置（二选一，两者等价）
cp config.example.json config.json
# 或
cp config.example.yml  config.yml

# 2. 改配置（至少填 redis）
#    加载配置不会回写文件；仅旧版本迁移会备份并更新文件

# 3. 准备 Redis（必须）
#    Windows: 下载 tporadowski/redis 解压后运行 redis-server.exe
#    Linux:   apt install redis / yum install redis

# 4. 编译并启动
cargo build --locked --profile=fast
./target/fast/biliroaming_rust_server
```

默认监听 `0.0.0.0:2662`，建议用 Nginx 反代。

> `web/index.html` 是**可选**的。若不存在，`/` 会返回内置占位页，不会报错。

## 2. 配置要点

### 2.1 `config_version` 必须是 4

加载与迁移均支持 `config.yml`、`config.yaml`、`config.json`，同时存在时按此顺序选择。建议只保留一份。

版本不高于 3 时，迁移仅在新字段缺失时将 `port` / `woker_num` 映射到 `http_port` / `worker_num`，保留已填写的新字段和凭据。写入前先验证新配置，并创建同目录 `config.<扩展名>.v3.bak`；已有备份不会被覆盖。正常加载不回写配置文件。

`worker_num` 必须大于 0。配置格式错误会报告错误并退出。

### 2.2 YAML 中枚举必须用 `!Tag` 语法

`blacklist_config` 与 `report_config` 是 serde 枚举，YAML 里**不能**照搬 JSON 的 map 写法：

```yaml
# 错误 → serde_yaml 报 invalid type: map, expected a YAML tag starting with '!'
blacklist_config:
  MixedBlackList:
    api: https://example.com/

# 正确
blacklist_config: !MixedBlackList
  api: https://black.qimo.ink/api/users/
  api_version: 2
```

可选值：`!OnlyLocalBlackList` / `!NoOnlineBlacklist` / `!OnlyOnlineBlackList` / `!MixedBlackList`。
`report_config` 同理：`!TgBot` / `!PushPlus` / `!Custom`。

### 2.3 凭据接口和可信代理

`/api/accesskey` 默认关闭。开放某个地区必须同时配置 `api_assesskey_open` 对应的 `1`～`4` 开关和非空 `api_sign`。未填写或留空不再随机生成密钥；开启接口却没有密钥时启动失败。返回凭据的响应使用 `Cache-Control: no-store`。

限流按客户端 IP 计算，不使用用户可修改的 access key，也不包含源端口。默认不信任任何转发头；反代部署需要填写反代与本服务连接所用的 IP，例如本机 Nginx：

```yaml
trusted_proxies: [127.0.0.1, '::1']
```

仅这些地址提供的单个合法 `X-Real-IP` 才会生效。反代必须覆盖客户端传入的同名头；未配置时反代后的用户共享反代 IP 的限流额度。原 `rate_limit_per_second` 仍表示补充一个令牌所需的秒数。

`access_key` 必须为完整的 32 位十六进制字符串，不再截断后缀。

### 2.4 TLS、凭据和缓存

`https_support: true` 时证书错误会阻止启动，不再退回 HTTP。私钥支持 PKCS#8、PKCS#1 RSA 和 SEC1 EC PEM 格式，文件路径仍为 `certificates/fullchain.pem` / `certificates/privkey.pem`。

`cn_resign_info`、`th_resign_info` 保留在配置中。启动只在 Redis 缺少对应记录时写入，避免覆盖已刷新 token；需要手动更换账号时也应处理对应 Redis 记录。

地区缓存改用 `e<ep_id>1402`，旧 `1401` 数据不再读取，无需清空 Redis。新地区缓存有效期为一小时；会员限制不代表地区不可用，临时上游错误不会写成不可用。Redis 缓存写入失败只记录不含凭据的警告。

后台同时执行最多 8 个任务，播放链接按缓存键合并刷新。队列满时丢弃新任务，后续请求可重试。

## 3. 代理配置（关键，易踩坑）

### 3.1 代理按区域独立配置

| 配置项 | 作用 |
|---|---|
| `hk_proxy_playurl_open` / `hk_proxy_playurl_url` | 香港区 playurl |
| `tw_proxy_playurl_open` / `tw_proxy_playurl_url` | 台湾区 playurl |
| `cn_proxy_*` / `th_proxy_*` | 大陆 / 泰区 |

地址格式：`127.0.0.1:7890`（默认按 socks5 解析），或显式带协议 `http://...`、`socks5://...`。
**留空字符串 `""` 时代理不生效**，即使 `_open = true`（`request.rs` 检查 `proxy_url.len() != 0`）。

### 3.2 ⚠️ 陷阱：指向 Clash/sing-box 混合端口往往静默失效

**这是实测踩到的坑。** 以 sing-box 为例，其默认规则集包含：

```
rule_set=system-geosite-cn         => route(direct)   # B 站命中这条
rule_set=system-geolocation-not-cn => route(proxy)
route.final = proxy
```

`api.bilibili.com` 匹配 `system-geosite-cn` → **强制直连**。此时把 `hk_proxy_playurl_url` 设为 `127.0.0.1:7890` 配置"成功"，但 B 站请求实际仍从本机 IP 发出，**解锁不生效且无任何报错**。

排查方法（看真实路由）：

```bash
curl http://127.0.0.1:9090/connections | grep bilibili
# chains=direct → 说明被规则排除在代理之外
```

**解决办法：为 B 站单独提供一个强制走代理的出口**。例如用 sing-box 起独立实例，只含目标地区节点且 `route.final` 指向它：

```json
{
  "log": { "level": "warn" },
  "inbounds":  [{ "type": "mixed", "tag": "in", "listen": "127.0.0.1", "listen_port": 7899 }],
  "outbounds": [{ "type": "shadowsocks", "tag": "out", "server": "<节点>", "server_port": 2377,
                  "method": "chacha20-ietf-poly1305", "password": "<密码>",
                  "plugin": "obfs-local", "plugin_opts": "obfs=tls;obfs-host=..." }],
  "route": { "final": "out" }
}
```

然后 `hk_proxy_playurl_url = "127.0.0.1:7899"`、`hk_proxy_playurl_open = true`。

验证出口：

```bash
curl -x http://127.0.0.1:7899 https://api.bilibili.com/x/web-interface/zone
# 期望: {"data":{"country":"台湾","isp":"cht.com.tw", ...}}
```

### 3.3 地区与出口必须匹配

| 目标 | 需要出口 |
|---|---|
| 港澳台番剧 | 香港 或 台湾 |
| 东南亚（泰区） | 泰国 / 东南亚 |
| 大陆限定 | 大陆（通常无需代理） |

出口不对会返回 `6002003 抱歉您所在地区不可观看！`。

### 3.4 反向代理（Nginx）示例

⚠️ **本节配置未在本仓库环境中实测**，是给部署者参考的起点模板。上线前请在目标机器上用 `nginx -t` 校验，并逐条验证下面列出的行为。

反代要同时满足三件事：终止 TLS、把真实客户端 IP 传给本服务、以及把 gRPC 路径转发到上游（见 10.2 说明为什么这项只能放在代理层）。

```nginx
# 上游 B 站 gRPC 端点，供下面的 location 转发使用
upstream bili_grpc_app  { server app.bilibili.com:443;   keepalive 16; }
upstream bili_grpc_net  { server grpc.biliapi.net:443;   keepalive 16; }

server {
    listen 443 ssl;
    http2 on;                      # 旧版 Nginx 用 `listen 443 ssl http2;`
    server_name your.domain;

    ssl_certificate     /etc/ssl/fullchain.pem;
    ssl_certificate_key /etc/ssl/privkey.pem;

    # 1) 真实客户端 IP：本服务只信任 trusted_proxies 里列出的地址提供的 X-Real-IP。
    #    反代必须覆盖客户端可能自带的同名头，否则限流身份可被伪造。
    #    同时把本机地址填进 config：trusted_proxies: [127.0.0.1, '::1']
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header Host $host;

    # 2) 常规 REST 路径交给本服务
    location / {
        proxy_pass http://127.0.0.1:2662;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_http_version 1.1;
    }

    # 3) gRPC 路径直接转发到上游（本服务无 HTTP/2 trailers 能力，无法自行构造 gRPC 响应）
    #    注意：这是纯透传，不带 resign 与区域代理，因此它只让新客户端不报错，并不解锁。
    location /bilibili.app.playurl.v1.PlayURL/ {
        grpc_pass grpcs://bili_grpc_net;
        grpc_set_header X-Real-IP $remote_addr;
    }
    location /bilibili.pgc.gateway.player.v1.PlayURL/ {
        grpc_pass grpcs://bili_grpc_app;
        grpc_set_header X-Real-IP $remote_addr;
    }
    location /bilibili.community.service.dm.v1.DM/ {
        grpc_pass grpcs://bili_grpc_app;
        grpc_set_header X-Real-IP $remote_addr;
    }
}
```

上线后建议逐条确认：

```bash
# TLS 与常规路径可达
curl -s https://your.domain/ | head -c 200

# 限流身份是否来自真实客户端：伪造 X-Real-IP 不应改变计数
curl -s -H 'X-Real-IP: 1.2.3.4' https://your.domain/x/v2/search/type?area=hk

# 出口地区是否正确（决定解锁是否生效）
curl -x http://127.0.0.1:7899 https://api.bilibili.com/x/web-interface/zone
```

若 `trusted_proxies` 未配置，本服务会忽略 `X-Real-IP` 并按直连地址限流；此时所有用户共享反代 IP 的额度。若反代未覆盖客户端自带的 `X-Real-IP`，攻击者可轮换该头绕过限流。

## 4. 获取 access_key（TV 扫码）

项目需要 `access_key`（不是 Cookie）。**B 站 TV 端登录接口会直接返回它**，且其 appkey 就写在代码里（`types.rs:479` 的 `AndroidTV`），无需逆向。

```bash
# 步骤 1：申请授权码
appkey=4409e2ce8ffd12b8
appsec=59b43e04ad6965f34319062b478f83dd
ts=$(date +%s)
qs="appkey=$appkey&local_id=0&ts=$ts"
sign=$(printf '%s' "$qs$appsec" | md5sum | cut -d' ' -f1)
curl -s -X POST 'https://passport.bilibili.com/x/passport-tv-login/qrcode/auth_code' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  -d "$qs&sign=$sign"
# 返回 {"code":0,"data":{"auth_code":"...","url":"https://passport.bilibili.com/x/passport-tv-login/h5/qrcode/auth?auth_code=..."}}

# 步骤 2：用手机 B 站 App 扫描上面的 url（或用二维码生成器渲染）

# 步骤 3：轮询换取 token
ts=$(date +%s)
qs="appkey=$appkey&auth_code=<上一步的auth_code>&local_id=0&ts=$ts"
sign=$(printf '%s' "$qs$appsec" | md5sum | cut -d' ' -f1)
curl -s -X POST 'https://passport.bilibili.com/x/passport-tv-login/qrcode/poll' \
  -H 'Content-Type: application/x-www-form-urlencoded' \
  -d "$qs&sign=$sign"
# code=0 → data.token_info.access_token 就是 access_key（32 位）
# code=86039 → 二维码尚未确认
# code=86038 → 二维码未扫描/已过期
```

**注意**：
- 二维码约 **3 分钟**过期，过期需重新生成
- 扫码会给账号新增一条 **TV 端设备登录记录**，可在 B 站 App「设置 → 设备管理」中踢掉
- 任何 B 站账号均可（**不需要大会员**）

---

# 第二部分：验证结论

## 5. 实测：B 站上游接口存活状态

| 链路 | 返回 | 判定 |
|---|---|---|
| `/pgc/player/web/playurl`（app） | 业务码（地区/登录限制） | ✅ 老签名仍有效 |
| `/pgc/player/api/playurl`（app） | 同上 | ✅ 有效 |
| `/x/v2/account/myinfo` | `-101 账号未登录` | ✅ 有效 |
| `/x/v2/search/type`（**app search**） | `code:0`，44962 字节 | ✅ 正常 |
| `/intl/gateway/v2/ogv/view/app/season` | `10003003 该地区无法访问` | ✅ 到达 intl 上游 |
| `/x/web-interface/search/type`（**web search**） | 上游 `-400` / `412` | ❌ 失效 |
| `/x/space/wbi/acc/info`（查 VIP 到期） | `-352 风控校验失败` | ❌ 需 wbi 签名 |
| `bangumi.bilibili.com/view/web_api/season` | `502` | ❌ 网关下线 |
| `black.qimo.ink`（在线黑白名单） | `code:0` + "即日起停止服务" | ⚠️ 停服但优雅降级 |

**核心结论**：老签名方案（appkey + appsec + MD5）仍被 B 站接受——playurl 返回的是**业务错误码**而非 `-3 签名错误`。服务主体逻辑没有死，失效的是周边依赖。

## 6. 端到端验收证据

### 6.1 地区限制确实被穿透

同一 `ep=653896`、同一老签名请求，仅切换出口：

| 出口 | `area=tw` | `area=hk` |
|---|---|---|
| **台湾** | `6002105 开通大会员观看` | `6002105 开通大会员观看` |
| 国内直连（对照） | `6002003 抱歉您所在地区不可观看！` | — |

`6002003`（地区限制）→ `6002105`（会员限制）说明**地区限制已被穿透**。

反向对照亦成立：大陆限定番剧在国内直连可正常取流，走台湾出口反而 `6002003`。**说明是真实的地区切换，而非无条件放行。**

### 6.2 免费番剧完整解锁（零大会员依赖）

选用台限免费番剧 `season_id=33088`「輝夜姬想讓人告白？（僅限台灣地區）」，`ep=318304`：

| 出口 | 结果 |
|---|---|
| 台湾 | `code=0 success`，15 档清晰度 |
| 国内直连 | `6002003 地区不可观看` |

**空 `access_key` 也放行**——完全不需要会员。

### 6.3 真实账号完整闭环

用非大会员账号（`due_date` 已过期，已确认）经服务请求：

```
服务(2662) → 台湾出口 → B 站
返回: code=0 success, 15 档清晰度 (112/80/64/32/16)
实际下载流: HTTP 206, 1MB 真实数据, 文件头 ftypiso5 (合法 MP4)

补充：免费锁区集(空 access_key)返回的流完整下载为 148 MB / HTTP 200，同样是合法 MP4。
```

## 7. ⚠️ 正确理解 `-10403 "检测到可能刚刚买了带会员"`

首次请求**可能**返回：

```json
{"code":-10403,"message":"其他错误: 检测到可能刚刚买了带会员, 刷新缓存中, 请稍后重试喵"}
```

**这不是 bug，是有意设计。** 触发条件（`upstream_res.rs:966-1015`）：

```
账号非大会员  且  拿到了 need_vip=True 的画质（限免/状态变动）
→ 判定为异常 → 拒绝本次请求，同时触发 ep_need_vip 缓存刷新
```

**实测验证**：同一集连续请求 3 次，第 1 次 `-10403`，**第 2、3 次均为 `code=0`**（缓存已写入）。客户端只需重试。

日志对应行：

```
[ERROR] ... EP <id> -> 非大会员用户获取了大会员独享视频, 可能大会员状态变动或限免, 并且尝试更新ep_need_vip失败
```

## 8. 环境事实（本次验证环境）

| 项 | 值 |
|---|---|
| 代理软件 | sing-box 1.14.1（satelite 客户端），mixed 入站 @ 127.0.0.1:7890 |
| 节点分布 | 44 个 shadowsocks：美国 24 / **台湾 11** / 日本 8 / 失败 1 |
| 台湾出口 IP 示例 | `211.22.161.167`、`211.23.97.150`、`60.249.35.96`（中华电信 cht.com.tw） |
| 无香港、无泰国节点 | 港澳台方向用台湾出口即可 |

---

# 第三部分：维护参考

## 9. 本仓库已修复的缺陷

全部经实测复现后修复。

| # | 位置 | 问题 | 修复 |
|---|---|---|---|
| 1 | `handler.rs:748` | `area_num` 非数字 → `parse().unwrap()` **panic，连接被断开** | 改 `match` + 返回 `-10403` |
| 2 | `request.rs:151/164` | Redis 不可用时 `redis.get().unwrap()` **panic** | 改返回 `None` 优雅降级 |
| 3 | `upstream_res.rs:1336` | 上游返回非 JSON（412 HTML）时 `data.json().unwrap()` **panic** | 改 `match` + 上报健康 |
| 4 | `upstream_res.rs:1506` | 泰区 season 改写路径同类 panic | 改透传原始内容 |
| 5 | `upstream_res.rs` 等 | 20 处字段级 `unwrap()`（`code`/`mid`/`uid`/`season_id`/`episodes` 等） | 全部改 `unwrap_or` / `match` |
| 6 | `upstream_res.rs:1732` | `contains_key("badge")` 判 VIP，但 B 站对免费集也返回空 `badge` 字段 → **所有集误判为 VIP** | 改判 `badge.contains("会员")` |
| 7 | `cache.rs:454` | 泰区 deadline 解析把 `\u0026` 替换成换行符（应为 `&`） | 修正替换目标 |
| 8 | `types.rs:2682` | `UserLoginInvalid` 用 `{{ }}` 转义写法 → 输出**非法 JSON** | 改单大括号 |

**badge 修复实测效果**：

| season | 修复前 | 修复后 | 实际 |
|---|---|---|---|
| 33088（免费） | 12/12 判 VIP ❌ | **0/12** ✅ | 全免费 |
| 42292 | 12/12 ❌ | **11/12** ✅ | 11 VIP |
| 32955（免费） | 6/6 ❌ | **0/6** ✅ | 全免费 |

> 注：该修复**不影响缓存行为**——`types.rs:191` 只写 `keys[0]`，`keys[1]`（non-vip 键）的写入代码在**注释里**从未执行。修复的真实收益是诊断日志变准确。

## 10. 已知失效与未修复项

| 项 | 状态 | 说明 |
|---|---|---|
| `web search` | ❌ 上游 `-400`/`412` | README 已标注 web 脚本弃用；**app search 正常** |
| gRPC 路径（`*.PlayURL` / `*.DM`） | ❌ 未实现，**受框架限制** | 客户端 5.37.0 起改用 gRPC。actix-web 4.15 无 HTTP/2 trailers API，无法发送 gRPC 必需的 `grpc-status`，故当前无法在进程内实现。见 10.2 |
| `wbi` 签名 | ❌ 未实现 | 影响 `get_upstream_bili_account_info_vip_due_date`；但 `due_date` 可从 myinfo 直接获取，非必需 |
| `bangumi.bilibili.com` | ❌ 502 | ep_info 主源下线，仅剩 `api.bilibili.com` 回退源 |
| `ep_info` 不走区域代理 | ⚠️ 低影响 | `ep_info.rs:35` 硬编码 `proxy_open=false`，导致锁区集查不到 `ep_need_vip`。**但当前无代码消费该值做决策**（两个消费点均不依赖它），修了看不出效果 |
| `black.qimo.ink` | ⚠️ 已停服 | 建议改 `blacklist_config: !NoOnlineBlacklist` |
| `Area::new` panic | ⚠️ 不可达 | `types.rs:1869` 对非法 `area_num` panic。实测 `area_num=0/5/255` 均返回 `-404`（该路径会先归一化为 4 或 1），故不可达。已加测试记录现状 |

### 10.1 ⚠️ 一处「看似是 bug，实际不能改」

`handler.rs:809` 的 `for index in 1..=8usize` 与正则的 9 个捕获组不匹配，导致 `Some(9)`（`/pgc/view/v2/app/season`）永远不可达。

**我一度改成 `1..=9`，实测发现这是回归**：该路径会落到 `main.rs` 的 `_ =>` 分支，返回 **`-500 未预期的行为`** 并打错误日志——而 `main.rs` 在**所有**上游分支（main / patch-1 / bump_dep_version / add-grpc-request）中都**没有 `9 =>` 分支**，README 也标注该接口已弃用。

**即：修复它反而把良性的 `-404` 变成 `-500` 噪声日志。已回退，保持上游原行为。**

> 教训：改动前先确认目标分支是否存在，而不是只看"捕获组数量对不上"。

### 10.2 gRPC 路径为什么不能直接照搬

参考实现（`biliroaming-ts-server-vercel`）在 Next.js 的 `rewrites` 里把三条 gRPC 路径透传到上游。这对本项目**不可直接照搬**，原因有两层：

1. **框架不支持**。gRPC-over-HTTP/2 要求响应以 trailer 帧返回 `grpc-status`。实测 actix-web 4.15 与 actix-http 3.18.12 的 h2 层没有任何发送 trailers 的 API（全库检索 `trailers` 仅命中 h1 chunked 解析与 `TE: trailers` 头部常量），因此无法在本进程内构造合法 gRPC 响应。
2. **语义不同**。参考实现是纯透传，不带 resign、不带区域代理，所以它只让新客户端不报错，并不能解锁。要真正解锁需要自行实现 gRPC 编解码与业务分支。

可行的替代方案：在 Nginx/Caddy 等反向代理层把这三条路径直接转发到上游（代理层具备完整的 HTTP/2 trailers 能力），本项目继续只处理 REST 路径。**本次未实现，也未验证**，需要时另行评估。

## 11. 回归测试

```bash
cargo test --locked
# 含纯函数回归、配置迁移和本地模拟 Redis/HTTP 集成测试
```

覆盖：`Area` 映射、`check_ep_available` 全部错误码分支、`gen_aurora_eid`/`eid_to_mid` 往返、`EType` JSON 合法性。

**测试已抓到的真实行为**（勿"修正"为假设）：
- `eid_to_mid("")` 返回 `Ok("")`，而非报错
- `Area::new(0)` / `Area::new(5)` 会 panic

## 12. 常见错误码

| 码 | 含义 | 排查方向 |
|---|---|---|
| `-101` / `61000` | 账号未登录 / 无效用户态 | access_key 无效或过期（**登录门，非会员门**） |
| `-3` | 签名错误 | appkey/appsec 不匹配 |
| `-412` | 请求被拦截 / 风控 | 换 IP 或降低频率 |
| `6002003` | 地区不可观看 | 代理未生效或出口地区不对 |
| `6002105` | 需开通大会员 | 内容本身需要会员 |
| `-10403`（带"刚刚买了带会员"） | 缓存刷新中 | **重试即可**，见第 7 节 |
| `-500 上游返回非JSON` | 上游被风控 | 见日志中的上游返回内容 |

## 13. 审查修复、依赖升级与验证范围

### 13.1 行为修复（2026-10-01）

覆盖凭据接口开关、空签名、请求参数 panic、限流绕过、Redis 写入 panic、地区判定、凭据保存、TLS 加载、日志凭据、分地区 resign、自定义 POST 通知及后台任务并发。CI 执行格式检查、测试和 Clippy；制品上传/下载升级为 v4，Cargo.lock 纳入版本控制。

### 13.1.1 与参考实现的对照修复（2026-10-01）

对照 `bili-vd-bak/biliroaming-ts-server-vercel` 后完成两项：

- **修复配置错误导致 panic**：`appsearch_remake` / `websearch_remake` 中的非法 JSON 原先走 `serde_json::from_str(..).unwrap()`，一个错别字即让工作线程 panic。现改为 `parse_search_remake()` 返回错误，记录日志并回退到上游原始响应。
- **新增内容屏蔽**：新增 `block_bangumi_ep` / `block_bangumi_cid` / `block_bangumi_avid` / `block_bangumi_bvid` 四个配置项，命中的请求在任何上游调用之前即被拒绝（返回 `-10403`）。默认全空，不影响现有配置。

未采纳的项：`try_unblock_CDN_speed`（改写 `bw=`，有效性未经验证）、PG/Notion 黑白名单（对个人自用属过度设计）、参考实现的 admin 接口（其密钥默认为空串，未配置环境变量时任何人可清空缓存）。

### 13.1.2 拒绝响应缓存与反代模板（2026-10-01）

- **静态拒绝响应加 30 秒缓存**：新增 `build_static_refusal_response!` 宏，用于结果不随配置变化的拒绝（请求格式错误、UA 不合法、签名错误、客户端版本过旧、参数缺失），降低客户端重试风暴。**黑名单、白名单、内容屏蔽与凭据接口不使用该宏**——它们必须随配置改动立即生效；凭据响应保持 `no-store`。
- **修复测试 mock 的 socket 竞态**：`tests/hardening.rs` 的 mock 服务器原先在写入后直接丢弃 socket，客户端在途数据未读完时会触发 RST，导致偶发失败。现改为写入后 `flush`、显式 `shutdown` 发 FIN、排空在途数据后再释放；同时补上 chunked 请求体解析。
- **新增反代配置模板**：见第 3.4 节，含 TLS 终止、`X-Real-IP` 覆盖与 gRPC 路径转发。**该模板未在本仓库环境实测**。

### 13.2 依赖升级（2026-10-01）

HTTP/TLS 依赖栈已迁移：`reqwest 0.12`（rustls-tls + http2）、`rustls 0.23`、`rustls-pki-types 1.x`、`actix-web 4.15`、`actix-governor 0.10`，并移除停维的 `rust-crypto`（改用 `md5` crate 实现上游要求的 MD5 签名）。

`h2 0.3.27` 仍是 Actix HTTP 的传递依赖，其官方修复版本 0.4.16 与 h2 0.3 接口不兼容，无法直接升级。因此仓库内 `vendor/h2/` 保留了官方修复的等价回移（`RUSTSEC-2026-0258` / `GHSA-q83h-524g-xf6h`）：256 字节开销阈值、25,600 字节连接预算、消费 DATA 后归还预算、丢弃非末尾空 DATA、超预算时以 `ENHANCE_YOUR_CALM` 关闭连接。补丁范围与来源见 `vendor/h2/SECURITY-PATCH.md`，源码摘要记录在 `vendor/h2/backport.json`。

### 13.3 依赖审计

`python3 scripts/audit-dependencies.py` 用 OSV 扫描 Cargo.lock，并把已验证的 h2 回移单独归档为 `verified_backports`；任何其他未解决告警都会使脚本以非零码退出，CI 因此失败。审计不会静默忽略告警。**当前扫描 319 个包，未解决项为 0。** Actix 上游采用已修复的 h2 版本后，应删除 `[patch.crates-io]` 覆盖。

### 13.4 验证边界

新增测试只使用模拟凭据和本机临时端口，不请求 B 站或推送平台。协议测试在真实 HTTP/2 帧层面验证空帧、小帧和带填充帧洪水会被拒绝，正常小数据流不受影响，且未被读取的请求体不会泄漏预算。当前实际运行的旧进程不会因修改源码自动更新，需要部署新二进制才会生效。
