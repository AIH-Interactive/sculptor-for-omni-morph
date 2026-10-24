# 装饰系统实施记录（独立后端）

依据客户端仓库 `ACCESSORY_SYSTEM_IMPLEMENTATION_PLAN.md`，按阶段 1～6 执行，禁止扩大范围。当前分支 `master`，基线 `a6b130b`；原 codex/accessory-system 已按用户要求合入并清理。

## 最新：阶段五自动验收 PASS（2026-10-24，待当前 master 提交推送）

整套 Apply/固定私有来源/观察者授权投影和旧基础投影已接通；复用原事务与认证，schema 11。独立 outfit-events 通知及 metadata 实例状态持久化接通，按配置/实例/固定来源/generation/sequence/state revision 隔离；恢复取基线，旧协议不消费新实例事件。apply/events 已公开 true。最终前台 cargo test 设置真实 Java classpath，187 PASS / 0 FAIL / 1 原有 ignored；编译32.61秒、测试26.11秒，父仓库 build/reports/outfit/stage5-final-backend.log。真实 Java OWNER CAS、原 Figura/Protobuf 观察者状态通知/读取/拒绝越权与重连定向2.29秒 PASS。真实 Minecraft 双客户端/GPU/GUI/光照/性能 MANUAL/PENDING，移交 S6。用户要求的局部收敛：缓存整理、批量授权和额外进程恢复延后，不新增验收门槛。主仓库 S5 检查点及远端 SHA 核对后再进 S6；历史早期 false 开关说明仅为当时快照。

## 阶段一：PASS（自动），人工 MANUAL/PENDING

- 独立 JSON v1 契约、资源用途/精确引用、稳定实例与骨骼路径、变换/预算/图校验。
- schema 6→7 备份/事务迁移；保留原列、三种格式、索引、策略 view、成员/候选/授权，支持空模型格式的预设目录内容。
- 上传/摘要/详情独立携带用途及版本装饰声明；骨架声明统一 CLIENT_EXTRACTED，不冒充服务端验证。
- 本人配置读取、预设 CAS/幂等保存读取及精确依赖索引；HTTP 账号隔离。组合 Apply/events 能力明确 false。
- `cargo test`：121 PASS，0 FAIL，1 原有 worker-saturation 测试按既有 ignored 规则跳过。含真实 HTTP 投稿、预设、迁移回滚、旧登录/单形象/收藏/下载/入集、冻结协议回归。
- `git diff --check` PASS；冻结协议未修改。验证日期 2026-10-21（Asia/Seoul）。
- 手工真实客户端/双客户端/GUI 均 MANUAL/PENDING；无渲染 PASS 声明。

阶段二客户端检查点已完成；阶段三自动验收 PASS，四～六 NOT STARTED。每阶段提交并确认推送后才进入下阶段。build.log、spm-cloud/ 为初始未跟踪内容，不提交。

## 阶段一用户补充：PASS（自动）

- 上限统一位于 `Config.example.toml` 的 `[outfit.limits]`，缺省键兼容旧配置；无效范围/未知字段拒绝。
- capabilities 从当前配置生成 `outfits_v1.limits`，保存预设和投稿声明消费同一预算；真实 HTTP 验证修改配置后限制生效、零容量、预设数量与 controls 拒绝。
- 动态光源仅锁定契约/预算：全局允许开关、每穿戴者光源数、最高亮度、最大半径；实际粒子照明及设置项按用户要求在阶段三实现。
- `cargo test` 124 PASS / 0 FAIL / 1 原有 ignored；追加动态光源边界用例后 `cargo test outfit_limits` 2 PASS。
- `git diff --check` PASS；版本/冻结协议未变。人工动态照明验收 MANUAL/PENDING。

## 阶段三检查点：PASS（自动，2026-10-22）

- 追加可选 editable_bones 声明与客户端内部覆盖契约，保留已有 exported_bones 授权边界；缺省空、路径/hash/重复/数量校验沿用既有机制。此前 outfit 服务 5 PASS / outfit HTTP 3 PASS，客户端进度记录保留对应日志。
- 用户增量 CustomParticle / Ribbon：增加 `[outfit.limits]` 的 max_custom_particles（默认 512，硬边界 4096）、max_particle_layers（4/8）、max_ribbon_vertices（4096/32768）、max_particle_callbacks_per_tick（64/1024）、max_particle_callbacks_per_frame（32/512），均支持 0；动态光源复用已有配置。旧配置缺省兼容，capabilities 使用当前配置下发。
- 前台 `cargo test ... outfit_limits` 3 PASS、`cargo test ... api::library::tests::outfits` 3 PASS，退出码 0；覆盖缺省、零值、安全边界、拒绝越界及真实 HTTP 配置下发。客户端保存 stage3-custom-particle-limits-backend.log / stage3-custom-particle-limits-http.log。
- 后端不传输逐粒子位置或顶点，未改冻结 Protobuf；客户端已实现挂点/内部覆盖、三种 CustomParticle 模式、运动/回调/动画事件、独立动态灯及隔离预览，详见父仓库阶段三验收表。真实光照/GPU/性能为 MANUAL/PENDING，按用户要求跳过。
- 最终前台 `cargo test`：126 PASS / 0 FAIL / 1 原有 ignored，日志 `target/outfit-reports/stage3-checkpoint.log`；定向 rustfmt --check、git diff --check PASS。原始 build.log 已按字节恢复，不提交初始未跟踪内容。
- 当前 master 检查点提交名 `feat(outfits): configure custom particle budgets and editable bones`；父仓库 main 提交并确认推送后才进入阶段四。Apply/events 授权跨端能力仍按阶段五接通。
