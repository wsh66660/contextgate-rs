# contextgate

**SQL ↔ 源码 静态索引**：扫描 Spring Boot + MyBatis 项目，把实际执行的 SQL 反查回源码位置（文件、行号、`Class#method`、调用者与上游路由）。

当应用不可运行、或你手里只有一条慢 SQL 日志时，contextgate 让你不再逐行翻代码：一条 SQL 进，源码位置出。

```
$ contextgate find my-spring-app "UPDATE users SET wallet_balance = 100.5 WHERE id = 42"

1. [1] UserMapper#addBalance — annotation-update [UPDATE]（src/main/java/com/demo/mapper/UserMapper.java:24）
   SQL 模板: UPDATE users SET wallet_balance = COALESCE(wallet_balance, 0) + #{delta} WHERE id = #{userId}
   涉及表: users
   触碰列: id, wallet_balance
   直接调用者: WalletController#recharge
   上游路由: POST /api/v1/wallet/recharge
   依据: 操作类型一致（UPDATE）；固定文本片段 2/2 命中（100%）；表匹配 100%；列匹配 100%
```

## 特性

- **归一化匹配引擎**：索引侧（源码模板）与查询侧（实际执行的 SQL）走同一套归一化——字符串/数字字面量、`#{}`、`${}`、`?` 统一折叠，`alias.col` 限定符统一剥前缀。核心信号是**固定文本片段召回**：执行 SQL 按 `?` 切出的固定片段必须逐字出现在源码模板中（模板是执行 SQL 的超集），辅以操作类型门控、表+别名、列包含度、结构相似度，让 SELECT 查询不会命中 INSERT 代码这类跨类型误报
- **全发射点覆盖**：
  - Mapper 注解 SQL（`@Select` / `@Insert` / `@Update` / `@Delete`）
  - MyBatis XML 语句（`<if>` / `<foreach>` / `<set>` 等动态标签拍平为形状）
  - JdbcTemplate 裸 SQL（字面量拼接 + 类级常量折叠 + 方法内局部 `String` 变量传播，不可解析成分折 `?`）
  - MyBatis-Plus 内置方法（`selectById` / `list` 等保守全列记录，`ServiceImpl<M,T>` 泛型基类绑定表归因）
- **源码定位**：每条命中带 `file:line`、`Class#method`、直接调用者、可达 HTTP 路由
- **零运行时依赖**：纯静态扫描，单文件二进制，秒级出索引
- **可持久化索引**：JSON 格式，可入库、可增量重建、可被任意工具消费

## 安装

### 从 Release 下载

```bash
# macOS (Apple Silicon)
curl -LO https://github.com/wsh66660/contextgate-rs/releases/latest/download/contextgate-darwin-arm64.tar.gz
tar -xzf contextgate-darwin-arm64.tar.gz && sudo mv contextgate /usr/local/bin/

# Linux x86_64（musl 静态链接，无 glibc 依赖）
curl -LO https://github.com/wsh66660/contextgate-rs/releases/latest/download/contextgate-linux-x86_64.tar.gz
tar -xzf contextgate-linux-x86_64.tar.gz && sudo mv contextgate /usr/local/bin/
```

### 从源码构建

```bash
git clone https://github.com/wsh66660/contextgate-rs.git
cd contextgate-rs && cargo build --release
# 二进制在 target/release/contextgate
```

## 使用

```bash
# 1. 扫描项目，生成索引（默认 <项目根>/.contextgate/index.json）
contextgate scan <项目根> [-o index.json]

# 2. 用实际执行的 SQL 反查源码位置
contextgate find <项目根> "<SQL>"
# 也接受 index.json 路径：
contextgate find <项目根>/.contextgate/index.json "<SQL>"
```

代码变更后重新 `scan` 一次即可刷新索引（秒级）。

> **服务端集成**：CI 流水线扫描上传 + Spring Boot/MySQL 提供 find 接口的完整对接方案
> （含 gzip 传输、索引瘦身与去重、多实例缓存、`find --json` 契约）见
> [docs/spring-boot-integration.md](docs/spring-boot-integration.md)。

### 匹配打分

| 信号 | 权重 | 说明 |
|---|---|---|
| 固定文本片段召回 | 45% | 查询按 `?` 切出的 ≥8 字符片段在记录模板中的命中率（限定符剥前缀后逐字比对）。慢日志里的 SQL 是模板渲染结果，片段命中即近必然来源 |
| 表 + 别名 | 30% | 表集合 Jaccard；双方都有 `FROM t a` 形态时混合 (表,别名) 对 Jaccard |
| 列包含度 | 15% | 查询列在记录列中的覆盖率（记录列更多不惩罚） |
| 结构相似度 | 10% | 只比较真实标识符（表/列名）的 Jaccard，剔除样板关键字与 `?` 等无判别力 token |

查询太短没有片段时回退旧权重（表 50% / 列 25% / 结构 25%）。

门控因子（乘在总分上）：

- **操作类型不符**（SELECT 查询 vs INSERT 记录）×0.15 强降权——同一发射点几乎只发同一类语句
- 表完全不重合 ×0.5 软降权（保留可见性，不误杀表提取失败的正确记录）
- MyBatis-Plus 内置方法的保守记录（`SELECT *` 全列上界）×0.6，避免虚高抢位
- 动态 SQL 模板 ×1.05 略补偿

## 工作原理

### scan：索引是怎么建的

纯静态、不编译项目、不连数据库。三遍扫描 + 一次逆向索引：

**第一遍 — 类解析与实体→表映射**
遍历所有 `.java` 文件，用线性扫描器（无回溯、非完整 AST）解析出类骨架：package、注解、字段、方法、常量。识别出实体类（`@TableName` 注解，或包名含 `entity/domain/model/po`）后建立 `实体 → 表` 映射：注解值优先，否则 camelCase→snake_case（`OrderItem` → `order_item`）。

**第二遍 — SQL 记录 + 调用图 + 路由**（逐类）
- **注解 SQL**：`@Select/@Insert/@Update/@Delete` 的字符串参数直接提取；
- **JdbcTemplate**：扫 `query/update/execute(...)` 调用点取首参字符串表达式——字面量拼接、类级常量引用、方法内局部 `String` 变量逐链折叠，解析不了的成分折成 `?`；
- **MyBatis-Plus 内置**：识别 `baseMapper.selectById(...)` / `service.list(...)` 等调用，从 `ServiceImpl<M, T>` 泛型链或实体字段类型绑定表，产出 `SELECT * FROM t` 形态的保守记录（标 `mp_builtin`）；
- **调用图**：方法体内的 `a.b(...)` 调用收集为 `调用者#方法 → 被调者#方法` 边，Mapper 接口方法是图的汇点；
- **路由**：controller 类的 `@RequestMapping/@GetMapping/...` 扫出 `METHOD path → Controller#handler`。

**第三遍 — MyBatis XML**
含 `<mapper` 的 `*.xml` 全部解析：提取 `<select>/<insert>/<update>/<delete>` 语句，动态标签拍平（`<if>` 留体内文本、`<foreach>` 展开一次迭代形状、`<include refid="..."/>` 展开同文件 `<sql id>` 片段），每条语句成为一条记录，`file:line` 取语句起始行。

**逆向索引**
调用图翻转后，对每条 SQL 记录节点向上 BFS：全部祖先即潜在调用者，祖先中的 `Controller#handler` 节点在路由表里查得可达 HTTP 路由。`find` 命中时能报「直接调用者 + 上游路由」就是这一步的产物——一条慢 SQL 直接追到对外入口。

结果写入 `.contextgate/index.json`（格式见 [索引格式](#索引格式)）。

### find：反查是怎么做的

核心思想：**同一套特征提取跑在两侧**——索引侧（源码模板）与查询侧（实际执行的 SQL）走同一归一化，反查退化为一次打分检索而非字符串匹配。

**第 1 步 — 归一化**
两侧走同一 `skeleton_text`：字符串字面量、数字、`#{}`/`${}`/`?` 占位符统一折 `?`，去反引号，剥 `alias.col` 限定符前缀（`t.id` → `id`），统一小写与空白。token 流上另提取：表集合、列集合、操作类型（首 token）、`(表,别名)` 对。

**第 2 步 — 逐记录打分**（信号与权重见 [匹配打分](#匹配打分)）
- **固定文本片段召回（主信号）**：查询按 `?` 切成 ≥8 字符片段，与记录模板做逐字比对。慢日志里的 SQL 就是模板的渲染结果，片段命中即近必然来源；动态模板是执行 SQL 的超集（含所有 if 分支），执行片段必然出现在其中；
- **表+别名 / 列包含度 / 结构相似度**：次级确认信号；
- **门控**：操作类型不符 ×0.15（SELECT 查询不会命中 INSERT 记录）、表零重合 ×0.5、MP 内置保守记录 ×0.6、动态模板 ×1.05；
- 表/结构/片段三者同时不重合的记录提前剪枝，总分 ≤0.03 丢弃。

**第 3 步 — 排序与渲染**
按分排序取 top 8。每条命中附带逆向索引信息：SQL 模板、涉及表/列、直接调用者、上游路由，以及各打分依据（可审计「为什么是它」）。

## 索引格式

`scan` 产出 JSON 地图，核心结构：

```jsonc
{
  "project": "my-app",
  "generated_at": "2026-10-27T10:54:00Z",
  "records": [
    {
      "id": "UserMapper#addBalance",
      "kind": "annotation-update",        // annotation-*/xml/jdbc-template/mp-builtin
      "sql": "UPDATE users SET ...",      // SQL 模板（动态标签已拍平）
      "skeleton": ["update", "users", ...], // 归一化 token
      "tables": ["users"],
      "columns": ["id", "wallet_balance"],
      "file": "src/main/java/.../UserMapper.java",
      "line": 24,
      "owner": "UserMapper",
      "method": "addBalance",
      "mp_builtin": false,
      "dynamic": true
    }
  ],
  "call_graph": [ { "caller": "A#m1", "callee": "B#m2" } ],
  "routes": [ { "method": "GET", "path": "/api/v1/...", "controller": "...", "handler": "..." } ],
  "entities": [ { "entity": "User", "table": "users", "file": "..." } ],
  "reverse": { "UserMapper#addBalance": { "direct_callers": [...], "routes": [...] } }
}
```

`records` 数组即「SQL ↔ 源码」索引本身，可被外部工具直接消费。

## 架构

```
src/
├── main.rs       # CLI：scan / find
├── model.rs      # 数据模型（SqlRecord / CallEdge / Route / Index）
├── scan/
│   ├── mod.rs    # 扫描编排 + 逆向索引（调用图 BFS → 路由）
│   ├── java.rs   # Java 扫描（类/字段/方法/常量/注解，线性扫描无回溯）
│   └── xml.rs    # MyBatis XML 解析 + 注解 SQL/路由补扫
├── sqlfeat.rs    # SQL 归一化：skeleton_text、字面量折叠、表/列/别名提取、片段召回
├── matcher.rs    # 打分匹配 + 结果渲染
└── store.rs      # 索引 JSON 读写
```

## 对接 AI Agent（MCP + Skill）

contextgate 是单文件 CLI，对接 agent 有两条路线，按 agent 的能力选：

| 路线 | 适用 | 复杂度 |
|---|---|---|
| **A. CLI + Skill** | 能执行 shell 命令的 agent（Claude Code、atomcode 等） | 无需服务，放一个 Skill 文件即可 |
| **B. MCP Server** | 无 shell 的 agent / 团队级工具分发 | 需跑一个 stdio MCP 服务 |

两条路线可以叠加：MCP 提供调用通道，Skill 教 agent「何时用、怎么解读输出」。

### 路线 A：CLI + Skill（推荐）

二进制装进 PATH 后，把下面的 Skill 放进 agent 的 skills 目录（Claude Code：`~/.claude/skills/`；atomcode：`~/.atomcode/skills/` 或项目内 `./.atomcode/skills/`），目录名 `slow-sql-locator`：

```markdown
---
name: slow-sql-locator
description: 用 contextgate 把 Spring Boot/MyBatis 项目里实际执行的 SQL 反查到源码位置。用户给出慢 SQL、SQL 日志并问「这段 SQL 是哪发的 / 谁在查这张表 / 对应哪个接口」时使用。
---

# 慢 SQL → 源码位置

## 步骤
1. 确认索引：`<项目根>/.contextgate/index.json` 存在；缺失或用户说代码改过，先 `contextgate scan <项目根>`（秒级）
2. 反查：`contextgate find <项目根> "<SQL>"`——SQL 原样粘贴（含参数值），不要剥别名、不要重排列：匹配引擎自己折叠参数，越原样命中越准
3. 解读输出并给结论：
   - top1 分数 ≥ 0.8 且依据含「固定文本片段 1/1 命中」：命中，直接报 file:line + Class#method + 直接调用者 + 上游路由
   - 分数 0.4–0.8 或多个候选分差 < 0.1：列出 top3 让用户确认，不要擅自选
   - 命中带 ⚠️（操作类型不符 / MP 内置 / 动态模板 / 运行期拼参）：说明告警含义，置信度相应降级
   - 无命中：先核对 SQL 的表名是否属于本项目；必要时退化为按表名 grep 源码
4. 「哪些代码在查 X 表」类问题：scan 后直接查 index.json 的 `records[].tables`

## 注意
- find 最多返回 8 条；多环境重复 XML 可能让近重复语句都上榜，属正常
- 本工具只定位不改代码；定位后是否优化索引/改写 SQL 交给用户决策
```

### 路线 B：MCP Server（内置，零安装）

MCP server 是**内置子命令**，Rust 原生实现（stdio 传输，换行分隔 JSON-RPC 2.0，手写协议层无 SDK 依赖），无需 Python/Node 环境：

```bash
contextgate mcp   # 启动后监听 stdin，把两个 tool 暴露给 MCP 客户端
```

暴露的 tool：

| Tool | 参数 | 说明 |
|---|---|---|
| `contextgate_scan` | `project_root`（可选 `out`） | 扫描项目建索引，秒级返回摘要 |
| `contextgate_find` | `target`（项目根或 index.json 路径）、`sql`（原样粘贴） | 反查源码位置，返回渲染文本 |

MCP 客户端配置（Claude Desktop `claude_desktop_config.json` / Cursor `mcp.json` / atomcode 的 MCP 配置，结构一致）：

```json
{
  "mcpServers": {
    "contextgate": {
      "command": "contextgate",
      "args": ["mcp"]
    }
  }
}
```

重启 agent 后它会看到两个 tool。tool 描述里写清了调用时机与「SQL 原样粘贴」的约束，agent 才能在对的场景主动使用——描述质量直接决定调用率。若要用 Skill 进一步约束解读方式，把路线 A 的 `slow-sql-locator` Skill 一起装上。

协议实现见 `src/mcp.rs`：支持 `initialize` / `tools/list` / `tools/call` / `ping`，tool 执行失败按协议以 `isError` 结果返回（客户端可直接把错误展示给模型）。

## 已知边界

- 静态分析的固有极限：真正的运行期拼参只能折成 `?`（表/列归因仍有效），命中的记录会标 `⚠️ 含运行期拼参`
- MyBatis-Plus 内置方法给的是「全列触碰」保守上界（`*`）
- 未覆盖（后续版本）：Wrapper 链式条件还原、Example 动态条件、跨类常量三链折叠、Feign 跨服务调用边
- 正则/线性扫描而非完整 AST：极端代码形态（泛型嵌套超过两层等）可能漏识别

## 背景

本仓库是 [ContextGate](https://github.com/wsh66660/ContextGate)（Python 版，4100+ 行分析器）的 Rust 重构。Python 版的 `find_sql` 只做子串匹配，无法用实际执行的 SQL 反查；Rust 版的核心增量是**归一化匹配引擎**——同一套特征提取跑在索引两侧，让「慢 SQL → 源码位置」成为一次打分检索。

## License

MIT
