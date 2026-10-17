# OmniMorph 后端启动、迁移与回滚

后端仓库：[sculptor-for-omni-morph](https://github.com/AIH-Interactive/sculptor-for-omni-morph)。
客户端默认 Protobuf；本后端同时提供 Figura Cloud 旧接口与 Protobuf v1。
实体模型的外部世界权威未接入，相关发布返回 AUTHORITY_UNAVAILABLE。

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
cloud-v1/manifest.json 的 schema_version 当前为 1；未知版本拒绝启动，不猜测性改写。
其中保存资源历史、装备/兼容引用、删除 tombstone、幂等操作及 YSM 快照/水位/恢复尾部。
blobs 保存按原始 SHA-256 命名的分发字节，uploads 保存未提交的临时文件。
Figura 历史 hash 是 SHA-256(Base64(raw))，与原始文件 hash 不同，不可相互替代。

文件校验与 blob 发布完成后才原子替换元数据；不能把上传授权当作提交成功。
进程在元数据替换前终止时，重新启动读取上一份已提交 manifest；不发布半成品。
重启清理旧 uploads/*.part，因为上传授权不跨进程保留；不可变但未被引用的 blob 保留，
不在启动期间冒险清理历史数据。服务重启后客户端重新认证，旧会话不复用。

## 关闭新入口与回滚

将 Config.toml 顶层 `protobufEnabled = false` 后重启，关闭 Protobuf HTTP、资源及 WS 入口。
旧配置缺少此字段时默认 true。关闭入口不删除 cloud-v1；Figura Cloud 旧入口仍可用。
客户端遇到不支持的后端会提示手动选择 Figura Cloud，不自动更换协议或域名。
用户显式选择会保存。既有本地模型在后端不可用时仍可加载。

需要回滚旧进程时先停新服务，使用备份配置与保留的旧 .moon 文件启动已核对的旧版本。
保留整个 cloud-v1，不让旧版本写入或“转换”其内容；旧版本忽略该目录。
新协议单独上传的包和 YSM 恢复记录不会被旧版本识别，待恢复新版本后重新使用；
不要把 YSM 或新数据强行覆盖成旧兼容文件。若需恢复旧配置/文件，也保留回滚期间的独立备份。

自动交付门禁实际启动了改造前 28fb81e 的后端、新后端入口关闭/开启进程，
用临时认证提供方与合成模型检查旧接口和 cloud-v1 全文件哈希保持；
也在同步元数据候选后真实终止子进程并重启，检查资源、装备、快照水位与 tombstone。
这些测试不代表公网切换或真实游戏渲染已通过。
