# Spring Boot 服务对接指南

本文面向在 Spring Boot + MySQL 服务端集成 contextgate 的开发（可交给 AI 编码助手执行）。
覆盖：CI 流水线扫描上传 → MySQL 事实源 → 多实例本地缓存 → find 接口（子进程调用）。

```
CI 流水线: 构建 → contextgate scan → POST /api/contextgate/index/upload（gzip JSON）─┐
                                                                                     ▼
              MySQL（唯一事实源）: cg_index_blob(project, version, json LONGBLOB)
                                                                                     │ 惰性拉取（每实例每版本一次）
        ┌────────────────────┬────────────────────┐                                   │
        ▼                    ▼                    ▼                                   ▼
   实例 A                实例 B                实例 N
   本地磁盘缓存 index.json（key: project + version）
        │
        └─ find: contextgate find --json <本地文件> "<SQL>"   ← 常态 0 网络开销
```

核心原则：

1. **匹配引擎只有一份实现**（contextgate 二进制），Spring Boot 只做存储、缓存、鉴权与转发
2. **MySQL 是唯一事实源**，实例本地磁盘只是缓存——任何一个实例挂了不丢数据
3. **find 每请求零下载**：只有「某实例 × 某索引版本」第一次被调用时才拉取

---

## 1. 环境要求

| 项 | 要求 |
|---|---|
| contextgate 二进制 | >= 0.1.2（需支持 `find --json`），放在服务机 PATH 或配置绝对路径；musl 静态链接，无任何运行时依赖 |
| MySQL | 无特殊要求；单项目索引压缩后百 KB 级 |
| Spring Boot | Web 任意版本；不需要额外中间件（Redis 可选） |

下载二进制（以 linux-x86_64 为例）：

```bash
curl -LO https://github.com/wsh66660/contextgate-rs/releases/latest/download/contextgate-linux-x86_64.tar.gz
tar -xzf contextgate-linux-x86_64.tar.gz && sudo mv contextgate /usr/local/bin/
```

---

## 2. 上传接口（接收 CI 的 index.json）

### 接口约定

```
POST /api/contextgate/index/upload?project={name}
Content-Type: application/json
Content-Encoding: gzip        ← CI 侧压缩上传（索引可从 5MB 压到 ~500KB）
Body: <gzip 后的 index.json 字节>
```

- **gzip 约定**：上传体一律 gzip 压缩（Rust 工具不感知压缩，服务端收到后解压）
- **幂等**：同一项目重复上传 = 版本切换；用事务「先删后插」或版本号切换，失败不影响旧数据
- **鉴权**：上传端点必须校验 CI 侧凭证（token / 内网限制）——它能覆盖服务上的索引数据

### MySQL 表

```sql
CREATE TABLE cg_index_blob (
  id         BIGINT AUTO_INCREMENT PRIMARY KEY,
  project    VARCHAR(128) NOT NULL UNIQUE,   -- index.json 的 project 字段（或查询参数）
  version    VARCHAR(32)  NOT NULL,          -- index.json 的 generated_at，实例缓存校验用
  json_gz    LONGBLOB     NOT NULL,          -- gzip 压缩后的原始 JSON（find 落盘的数据源）
  record_count INT, edge_count INT, route_count INT,
  uploaded_at DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 可选：结构化归档表（管理界面 / 跨项目检索用，find 不依赖）
CREATE TABLE cg_sql_record (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  project_id BIGINT NOT NULL,
  record_id VARCHAR(256) NOT NULL,           -- "UserMapper#addBalance"
  kind VARCHAR(32), verb VARCHAR(8),
  sql_text MEDIUMTEXT,
  file VARCHAR(512), line INT,
  owner VARCHAR(128), method VARCHAR(128),
  mp_builtin TINYINT(1), dynamic TINYINT(1),
  tables_json JSON, columns_json JSON,
  UNIQUE KEY uk (project_id, record_id)
);
```

> 注意：`sql_text` 必须存**解压还原后的模板原文**，不要存 `@p{n}` 占位符——占位符
> 是索引文件内部的去重手段，解析 index.json 时请先按 `sql_pool` 还原
> （`records[].sql == "@p0"` → 取 `sql_pool["@p0"]`）。

### 上传处理伪代码

```java
@PostMapping(value = "/api/contextgate/index/upload", params = "project")
public ResponseEntity<?> upload(@RequestParam String project,
                                @RequestHeader(value = "Content-Encoding", required = false) String enc,
                                InputStream body) throws IOException {
    byte[] gz = body.readAllBytes();
    byte[] json = "gzip".equalsIgnoreCase(enc) ? gunzip(gz) : gz;

    JsonNode root = objectMapper.readTree(json);
    String version = root.path("generatedAt").asText();   // 版本 = generated_at

    // 事务：delete by project → insert (project, version, gz, counts)
    indexRepo.replaceProject(project, version, gz, root);
    return ResponseEntity.ok(Map.of("project", project, "version", version));
}
```

---

## 3. find 接口（子进程调用 contextgate）

### 本地缓存（多实例 HA 的关键）

每实例维护 `project → (version, 本地文件路径)` 缓存：

```java
// 缓存条目
record CachedIndex(String version, Path file) {}

// find 流程（常态零下载）
public String find(String project, String sql) throws IOException {
    String latest = indexRepo.getLatestVersion(project);        // 主键点查 <1ms
    CachedIndex cached = cache.computeIfAbsent(project, p -> materialize(project, latest, indexRepo.fetchJsonGz(project)));
    if (!cached.version().equals(latest)) {                     // 新版本：原子替换
        cached = materialize(project, latest, indexRepo.fetchJsonGz(project));
        cache.put(project, cached);
    }
    // 子进程调用（无 shell，参数数组传参，无注入面）
    Process p = new ProcessBuilder(
            contextgateBin, "find", "--json", cached.file().toString(), sql)
        .redirectErrorStream(false)
        .start();
    String out = new String(p.getInputStream().readAllBytes(), UTF_8);
    if (!p.waitFor(3, TimeUnit.SECONDS)) { p.destroyForcibly(); throw new TimeoutException(); }
    if (p.exitValue() != 0) throw new IllegalStateException(new String(p.getErrorStream().readAllBytes(), UTF_8));
    return out;   // 直接作为 HTTP 响应体返回（已是 JSON）
}

// 原子落盘：写临时文件后 ATOMIC_MOVE，避免并发 find 读到半截文件
private CachedIndex materialize(String project, String version, byte[] gz) throws IOException {
    Path dir = Path.of(System.getProperty("java.io.tmpdir"), "contextgate", project);
    Files.createDirectories(dir);
    Path tmp = dir.resolve("index.json.tmp"), file = dir.resolve("index.json");
    Files.write(tmp, gunzip(gz));
    Files.move(tmp, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
    return new CachedIndex(version, file);
}
```

要点：
- **版本查询降频**（可选）：本地记 `lastVersionCheck` 时间戳，5 秒内的 find 直接用缓存版本——流水线扫描频率是分钟级，窗口足够
- **版本发布瞬间**各实例可能短暂混跑新旧版本，无碍：find 结果自带 `scannedAt`，前端可标注
- **缓存目录**放 `java.io.tmpdir` 即可（丢了会自动重新物化）

### HTTP 契约

```
POST /api/contextgate/find
{ "project": "my-app", "sql": "SELECT id, app_name FROM xxl_job_group WHERE id = 5" }

→ 200（body 即 contextgate find --json 的输出，原样透传或再包装）
```

**sql 必须原样粘贴**（含参数值、别名、限定符）——匹配引擎自己折叠字面量，
越原样「固定文本片段」命中率越高。不要在 Java 侧做任何 SQL 改写/参数剥离。

### `find --json` 输出结构（对接契约）

```jsonc
{
  "project": "xxl-job",
  "scannedAt": "2026-10-29T03:00:00Z",     // 索引版本，透传给前端判断时效
  "query": "SELECT ...",
  "hits": [                                 // 按分排序，最多 8 条；空数组 = 无命中
    {
      "score": 1.0,                         // 0~1；>=0.8 且片段命中即近必然来源
      "recordId": "XxlJobGroupDao#findAll",
      "kind": "xml",                        // annotation-select/update/... | xml | jdbc-template | mp-builtin
      "verb": "select",                     // select/insert/update/delete（null = 未知）
      "sql": "SELECT ... 模板原文",
      "tables": ["xxl_job_group"],
      "columns": ["id", "app_name"],
      "file": "src/.../XxlJobGroupMapper.xml",
      "line": 24,
      "owner": "XxlJobGroupDao",
      "method": "findAll",
      "mpBuiltin": false,
      "dynamic": true,
      "callers": ["XxlJobServiceImpl#add", "..."],     // 直接调用者
      "routes": [                                       // 可达 HTTP 路由
        { "method": "GET", "path": "/jobgroup/pageList", "controller": "JobGroupController", "handler": "pageList" }
      ],
      "reasons": ["操作类型一致（SELECT）", "固定文本片段 1/1 命中（100%）", "表匹配 100%", "..."],
      "warnings": ["dynamic-template"]     // "mp-builtin"=保守全列记录；"dynamic-template"=动态模板形状近似
    }
  ]
}
```

解读规则（也可做成前端提示）：

| 信号 | 处理 |
|---|---|
| `score >= 0.8` 且 reasons 含 `固定文本片段 1/1 命中` | 近必然命中，直接展示 top1 |
| `score 0.4~0.8` 或多候选分差 < 0.1 | 展示 top3 让用户确认 |
| `warnings` 非空 | 附提示：「MP 内置方法为全列保守记录」/「动态 SQL 模板，实际执行随条件增减」 |
| `hits` 为空 | 提示核对 SQL 来源项目；索引可能过期（比对 `scannedAt`），必要时重跑流水线扫描 |

---

## 4. 部署与升级

- CI 流水线（每项目）：`contextgate scan <项目根> && gzip -c <项目根>/.contextgate/index.json | curl -X POST --data-binary @- -H "Content-Encoding: gzip" "$SVC/api/contextgate/index/upload?project=<name>" -H "Authorization: ..."`
- 服务升级 contextgate 二进制时**无需重建索引**（JSON 格式向后兼容：新版本可读旧索引）
- find 无状态、只读子进程，天然水平扩展；实例重启缓存自愈

## 5. 已知边界

- 索引是**构建时刻快照**：代码变更后必须重跑流水线扫描，`scannedAt` 供前端判断时效
- 多环境重复 XML（同一 mapper 多份部署目录）会出现近重复命中，`file` 可区分环境
- 匹配器权重/信号仍在演进：服务端只透传 JSON，引擎升级不需要改对接代码
