# contextgate

**SQL ↔ 源码 静态索引**：扫描 Spring Boot + MyBatis 项目，把实际执行的 SQL 反查回源码位置（文件、行号、`Class#method`、调用者与上游路由）。

当应用不可运行、或你手里只有一条慢 SQL 日志时，contextgate 让你不再逐行翻代码：一条 SQL 进，源码位置出。

```
$ contextgate find my-spring-app "UPDATE users SET wallet_balance = 100.5 WHERE id = 42"

1. [0.945] UserMapper#addBalance — annotation-update（src/main/java/com/demo/mapper/UserMapper.java:24）
   SQL 模板: UPDATE users SET wallet_balance = COALESCE(wallet_balance, 0) + #{delta} WHERE id = #{userId}
   涉及表: users
   触碰列: id, wallet_balance
   直接调用者: WalletController#recharge
   上游路由: POST /api/v1/wallet/recharge
   依据: 表匹配 100%；列匹配 100%；骨架相似 67%
```

## 特性

- **归一化匹配引擎**：索引侧（源码模板）与查询侧（实际执行的 SQL）走同一套归一化——字符串/数字字面量、`#{}`、`${}`、`?` 统一折叠，按「表 Jaccard + 列包含度 + 骨架相似度」打分排序
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

### 匹配打分

| 维度 | 权重 | 说明 |
|---|---|---|
| 表 Jaccard | 50% | 两边提取的表集合重合度 |
| 列包含度 | 20% | 查询列在记录列中的覆盖率（记录列更多不惩罚） |
| 骨架相似度 | 30% | 归一化 token 序列的 Jaccard |

MyBatis-Plus 内置方法的保守记录（`SELECT *` 全列上界）自动降权 0.6 倍，避免虚高抢位。

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
├── sqlfeat.rs    # SQL 归一化：字面量折叠、表/列提取
└── matcher.rs    # 打分匹配 + 结果渲染
```

## 已知边界

- 静态分析的固有极限：真正的运行期拼参只能折成 `?`（表/列归因仍有效），命中的记录会标 `⚠️ 含运行期拼参`
- MyBatis-Plus 内置方法给的是「全列触碰」保守上界（`*`）
- 未覆盖（后续版本）：Wrapper 链式条件还原、Example 动态条件、跨类常量三链折叠、Feign 跨服务调用边
- 正则/线性扫描而非完整 AST：极端代码形态（泛型嵌套超过两层等）可能漏识别

## 背景

本仓库是 [ContextGate](https://github.com/wsh66660/ContextGate)（Python 版，4100+ 行分析器）的 Rust 重构。Python 版的 `find_sql` 只做子串匹配，无法用实际执行的 SQL 反查；Rust 版的核心增量是**归一化匹配引擎**——同一套特征提取跑在索引两侧，让「慢 SQL → 源码位置」成为一次打分检索。

## License

MIT
