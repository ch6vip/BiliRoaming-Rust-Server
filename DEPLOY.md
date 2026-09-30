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
| 主要风险 | 周边依赖老化（部分 web 接口失效）+ 历史 panic 隐患 |
| 本仓库已做的修复 | 6 类 panic 加固、badge 误判、泰区 deadline、61000 JSON、回归测试 |

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
#    程序启动时会自动规范化并回写配置文件

# 3. 准备 Redis（必须）
#    Windows: 下载 tporadowski/redis 解压后运行 redis-server.exe
#    Linux:   apt install redis / yum install redis

# 4. 编译并启动
cargo build --profile=fast
./target/fast/biliroaming_rust_server
```

默认监听 `0.0.0.0:2662`，建议用 Nginx 反代。

> `web/index.html` 是**可选**的。若不存在，`/` 会返回内置占位页，不会报错。

## 2. 配置要点

### 2.1 `config_version` 必须是 4

`update_biliconfig()`（`src/mods/config.rs:123`）会做版本迁移：

```rust
if config["config_version"].as_i64().unwrap_or(3) <= 3 {
    config["http_port"] = config["port"].clone();
    config["worker_num"] = config["woker_num"].clone();   // 旧字段名
    config["config_version"] = 4;
}
```

若手写配置且 `config_version <= 3` 但用了新字段名 `worker_num`，迁移会把它覆盖成 `null`，而 `BiliConfig.worker_num: usize` 无 serde default → **启动 panic**：

```
panicked at src/mods/config.rs:54:
invalid type: null, expected usize
```

**字段名对照**：`http_port`（旧 `port`）、`worker_num`（旧 `woker_num`）。

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

### 2.3 `api_sign`

`/api/accesskey` 的签名密钥。**留空**（`""`）时启动随机生成，适合个人使用。

> 安全提示：该接口的访问控制**仅**依赖此签名；代码中的 `api_assesskey_open`（逐区域开关）**从未被读取**，是无效配置项。

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

## 11. 回归测试

```bash
cargo test --test regression
# test result: ok. 7 passed
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
