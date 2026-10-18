# OmniMorph 后端启动、迁移与回滚

后端仓库：[sculptor-for-omni-morph](https://github.com/AIH-Interactive/sculptor-for-omni-morph)。
客户端默认 Protobuf；本后端同时提供 Figura Cloud 旧接口与 Protobuf v1。
实体模型的外部世界权威未接入，相关发布返回 AUTHORITY_UNAVAILABLE。

## 模型库 alpha.2 上线检查

模型库使用 `/api/library/v1` JSON 扩展，不改变 Protobuf v1。部署客户端目标版本为
`1.0.0-alpha.2`，构建平台以客户端 `enabled_platforms=fabric` 为准。升级前停止服务，
备份实际启动目录中的 `data/metadata.sqlite3`（包括存在的 WAL/SHM）、整个头像资源目录和配置。
不要在运行服务时直接复制 SQLite 主文件。自动迁移前的 `pre-library-*.sqlite3` 是
SQLite backup 快照（文件名形如 `metadata.pre-library-<随机值>.sqlite3`），不能替代完整资源和运营配置备份。

`[model_library]` 配置 `max_upload_bytes` 和 `categories`，客户端每次投稿重新获取限额。
配置管理员使用 `[[model_library.admins]]` 的已认证玩家 UUID、玩家名及密码哈希。
使用 `python tools/library_admin_hash.py` 在本地遮罩输入生成哈希；不把密码放进命令行或仓库。
不存在管理员账户创建 GUI。配置删除/改密后旧管理令牌失效；记住登录仅保存可轮换刷新令牌，
客户端无法建立当前用户专属文件权限时会禁用该选项。

模型库私有当前装备默认不向他人展示；衣柜上传默认展示，其独立开关绑定资源版本。
屏蔽优先于所有展示选择，同内容副本和旧分发入口也须保持 SELF_ONLY。
举报不会自动屏蔽；申诉处理说明不会自动解除屏蔽。管理员解除来源屏蔽后，模型库条目
保持 PRIVATE，不自动重新装备；其他来源的有效屏蔽仍然生效。

schema 3→4 已在合成临时库验证旧策略保留、升级前备份、重复打开和未知版本拒绝。
回退到 schema 3 的后端必须使用匹配的升级前数据库与资源备份，不能让旧二进制打开 schema 4。
更早的原始 Figura 基线 `28fb81e` 已验证可使用保留的旧 `.moon` 文件运行并保留新增云端数据；
这不代表其可读 schema 4 或最新装备，也不代表生产切换已经执行。

自动进程检查覆盖旧基线、当前后端 Protobuf 开/关、当前 Figura Java 适配器及数据/字节保留。
真实公网端点、运营备份恢复演练和实际游戏窗口仍为 MANUAL/PENDING，按本期要求不阻塞开发检查点。

## 从源码直接启动

需要 Rust/Cargo 与当前 Windows Rust 工具链。进入后端仓库，不是客户端 Gradle 目录。
首次复制示例配置；已有 Config.toml 时保留并编辑原文件：

```powershell
Set-Location D:\UserData\Desktop\Project\Figura\omnm-cloud
if (-not (Test-Path -LiteralPath .\Config.toml)) {
    Copy-Item -LiteralPath .\Config.example.toml -Destination .\Config.toml
}
$env:RUST_CONFIG = (Resolve-Path -LiteralPath .\Config.toml).Path
& cargo run --locked 2>&1 | Tee-Object -FilePath build.log
$buildExitCode = $LASTEXITCODE
Get-Content -LiteralPath build.log -Tail 40
if ($buildExitCode -ne 0) { exit $buildExitCode }
```

Cargo 会自动完成必要的编译再启动，无需先手动执行 cargo build 或寻找 exe；
Rust 不提供跳过编译的原生源码解释执行。后续未改源码时会复用构建缓存。
需要优化运行时可将启动命令换为 `cargo run --release --locked`，仍由 Cargo 自动处理。
停止服务使用 Ctrl+C。健康检查为 `http://127.0.0.1:6665/health`，正常返回 `ok`。

未设置 RUST_CONFIG 时默认读取当前工作目录的 Config.toml；启动会读取 .env。
配置文件路径与数据路径相互独立，相对路径均相对于启动工作目录。
可用环境变量：RUST_CONFIG（配置）、AVATARS_FOLDER（默认 data/avatars）、
ASSETS_FOLDER（默认 data/assets）、LOGS_FOLDER（默认 logs）、RUST_LOG（默认 info）。
数据目录中包含云端元数据、资源和恢复状态，变更目录必须连同这些数据一起迁移。
不要以 Config.example.toml 作为长期配置直接运行，以免更新覆盖本地设置。

## 下载 Windows / Linux 可执行文件

仓库的 [Backend executables](https://github.com/AIH-Interactive/sculptor-for-omni-morph/actions/workflows/backend-build.yml)
工作流在 main/master 推送、版本标签推送和拉取请求时构建，也支持手动 Run workflow。
进入一次成功运行的 Summary，在 Artifacts 或下载链接中选择：

- `omni-morph-backend-windows-x86_64.zip`：Windows x86_64，内含 `sculptor.exe`。
- `omni-morph-backend-linux-x86_64.tar.gz`：Linux x86_64 GNU，内含保留执行权限的 `sculptor`。

产物保留 30 天，包含示例配置、部署说明、许可证与可执行文件的 SHA-256。
首次解压后复制 `Config.example.toml` 为 `Config.toml`，编辑配置后，在解压目录运行
Windows 的 `.\sculptor.exe` 或 Linux 的 `./sculptor`，无需安装 Rust 或自行编译。
Linux 构建使用 Ubuntu 22.04（glibc 2.35 / OpenSSL 3）；运行环境需要相容的系统库。
自定义配置可设置 `RUST_CONFIG`，规则与上面的源码启动方式相同。
下载 [Actions Artifacts](https://docs.github.com/en/actions/tutorials/store-and-share-data) 需要登录 GitHub。

旧 Rust/Release/Docker workflow 已替换为这一份原生双平台构建；每个平台先测试，
再 `cargo build --locked --release --target ...`，最后上传可直接下载的压缩包。
本地可用 PowerShell 7 执行相同脚本（仅支持本机平台）：

```powershell
& ./.github/scripts/build-backend.ps1 -Target x86_64-pc-windows-msvc
```

Linux 对应 target 为 `x86_64-unknown-linux-gnu`。构建日志位于 `build.log`；
构建失败时工作流另行上传日志，缺失可执行文件或上传包会令工作流失败。

## 本地与正式连接

Config.toml 的 listen 默认 0.0.0.0:6665，后端自身监听普通 HTTP/WS。
本地测试可改为 127.0.0.1:6665，关闭 assetsUpdaterEnabled 避免自动拉取上游资源。
客户端开启 Debug，并显式填写 `http://127.0.0.1:6665`；不要附加 /api 或 /ws。
选择 Protobuf，或为兼容旧服务器手动选择 Figura Cloud。两种模式共享同一个 HTTP/WS origin。
Debug 关闭后明文连接立即停止，不能通过重试或日志开关绕过。

正式部署通过可信证书的反向代理提供 HTTPS/WSS；客户端填写域名或 https://域名。
代理需要支持 WS Upgrade、Authorization 与协商子协议，保留原请求路径，尤其是
历史 /api//auth 与 /api//assets 双斜线，不能将它们重写为单斜线。
将后端监听限制在代理可达的私有地址；代理日志不要记录认证查询、凭证头或文件正文。
模型文件直接经过 HTTP/HTTPS，代理上传大小/超时需匹配后端配置与客户端上限。
limitations.maxAvatarSize 的单位是十进制 KB，示例值 100 较小；按实际分发包调整，
并保持在客户端 64 MiB 文件上限内。不能因开启 Debug 就关闭 TLS 信任或主机名检查。

## 迁移与备份

停止旧服务，备份实际 Config.toml/.env、整个 AVATARS_FOLDER、资源目录及运营配置。
保留备份时的后端提交、客户端提交、配置和包哈希。一次只运行一个写入同一数据目录的进程。
先在备份副本部署双协议后端，验证旧 Figura 访问，再验证 Protobuf 和混合客户端；
最后发布默认 Protobuf 的客户端。公网部署与真实双游戏验收需要另行记录。

旧 owner.moon 保留原字节，首次查询对应 owner 时惰性补建 cloud-v1 索引和不可变 blob，
不解密 YSM、不转换原文件。重复导入同一内容不增加资源 revision，也不覆盖已选 YSM。
data/metadata.sqlite3 保存全部持久业务元数据：用户资料/封禁/上传权限、UUID 与模型版本关联、
装备/兼容引用、删除 tombstone、幂等操作及 YSM 快照/水位/恢复尾部。SQLite user_version 当前为 4，
未知版本拒绝启动，不猜测性改写。旧 schema_version=1 的 manifest.json 首次启动自动事务导入，
保留原文件作为迁移来源；此后仅 SQLite 权威，不再双写旧 manifest。
data/avatars/models 保存按原始 SHA-256 命名的完整分发字节和历史 UUID .moon；data/avatars/upload 保存上传暂存。
Figura 历史 hash 是 SHA-256(Base64(raw))，与原始文件 hash 不同，不可相互替代。

所有上传入口（包括管理和临时模型入口）先写 upload/*.part，再校验、发布文件并提交元数据；
此目录不按 Figura/YSM 引擎分叉。文件校验与 blob 发布完成后才提交 SQLite 事务；不能把上传授权当作提交成功。
进程在 COMMIT 前终止时，重新启动读取上一份已提交 SQLite 状态；不发布半成品。
重启清理旧 upload/*.part，因为上传授权不跨进程保留；不可变但未被引用的 blob 保留，
不在启动期间冒险清理历史数据。服务重启后客户端重新认证，旧会话不复用。
备份时停止服务，完整备份 data/metadata.sqlite3 及其可能存在的 -wal/-shm、整个 AVATARS_FOLDER，不要只复制数据库主文件。
旧布局首次启动自动复制到新目录并保留旧文件，SQLite backup 包含 WAL；细节见 [SQLITE.md](SQLITE.md)。
AVATARS_FOLDER 自定义只改变模型/暂存根目录，SQLite 始终位于工作目录下的 data/metadata.sqlite3。
旧系统从未持久化的账号名字无法凭空恢复；已知模型 owner UUID 会生成不带登录权限的资料，
认证成功后补齐真实账号资料。上传令牌、连接、上传授权和临时请求状态仍只在内存中，不写入数据库。

## 关闭新入口与回滚

将 Config.toml 顶层 `protobufEnabled = false` 后重启，关闭 Protobuf HTTP、资源及 WS 入口。
旧配置缺少此字段时默认 true。关闭入口不删除 cloud-v1；Figura Cloud 旧入口仍可用。
客户端遇到不支持的后端会提示手动选择 Figura Cloud，不自动更换协议或域名。
用户显式选择会保存。既有本地模型在后端不可用时仍可加载。

需要回滚旧进程时先停新服务，使用备份配置与保留的旧 .moon 文件启动已核对的旧版本。
SQLite 之后的新增装备/删除/账号更改不会反向同步到旧 manifest；旧 manifest 仅代表迁移时刻。
保留整个 cloud-v1，不让旧版本写入或“转换”其内容；旧版本忽略该目录。
新协议单独上传的包和 YSM 恢复记录不会被旧版本识别，待恢复新版本后重新使用；
不要把 YSM 或新数据强行覆盖成旧兼容文件。若需恢复旧配置/文件，也保留回滚期间的独立备份。

自动交付门禁实际启动了改造前 28fb81e 的后端、新后端入口关闭/开启进程，
用临时认证提供方与合成模型检查旧接口和 cloud-v1 全文件哈希保持；
也在同步元数据候选后真实终止子进程并重启，检查资源、装备、快照水位与 tombstone。
这些测试不代表公网切换或真实游戏渲染已通过。
