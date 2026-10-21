# 装饰系统实施记录（独立后端）

依据客户端仓库 `ACCESSORY_SYSTEM_IMPLEMENTATION_PLAN.md`，按阶段 1～6 执行，禁止扩大范围。分支 `codex/accessory-system`，基线 `a6b130b`。

## 阶段一：PASS（自动），人工 MANUAL/PENDING

- 独立 JSON v1 契约、资源用途/精确引用、稳定实例与骨骼路径、变换/预算/图校验。
- schema 6→7 备份/事务迁移；保留原列、三种格式、索引、策略 view、成员/候选/授权，支持空模型格式的预设目录内容。
- 上传/摘要/详情独立携带用途及版本装饰声明；骨架声明统一 CLIENT_EXTRACTED，不冒充服务端验证。
- 本人配置读取、预设 CAS/幂等保存读取及精确依赖索引；HTTP 账号隔离。组合 Apply/events 能力明确 false。
- `cargo test`：121 PASS，0 FAIL，1 原有 worker-saturation 测试按既有 ignored 规则跳过。含真实 HTTP 投稿、预设、迁移回滚、旧登录/单形象/收藏/下载/入集、冻结协议回归。
- `git diff --check` PASS；冻结协议未修改。验证日期 2026-10-21（Asia/Seoul）。
- 手工真实客户端/双客户端/GUI 均 MANUAL/PENDING；无渲染 PASS 声明。

阶段二～六 NOT STARTED。每阶段提交并确认推送后才进入下阶段。build.log、spm-cloud/ 为初始未跟踪内容，不提交。

## 阶段一用户补充：PASS（自动）

- 上限统一位于 `Config.example.toml` 的 `[outfit.limits]`，缺省键兼容旧配置；无效范围/未知字段拒绝。
- capabilities 从当前配置生成 `outfits_v1.limits`，保存预设和投稿声明消费同一预算；真实 HTTP 验证修改配置后限制生效、零容量、预设数量与 controls 拒绝。
- 动态光源仅锁定契约/预算：全局允许开关、每穿戴者光源数、最高亮度、最大半径；实际粒子照明及设置项按用户要求在阶段三实现。
- `cargo test` 124 PASS / 0 FAIL / 1 原有 ignored；追加动态光源边界用例后 `cargo test outfit_limits` 2 PASS。
- `git diff --check` PASS；版本/冻结协议未变。人工动态照明验收 MANUAL/PENDING。
