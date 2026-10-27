//! 数据模型：索引中每一条记录的结构定义。
//!
//! 核心思想：扫描阶段把源码中所有可能产生 SQL 的「发射点」提炼为
//! `SqlRecord`（SQL 模板 + 表/列特征 + 源码位置），查询阶段把一条
//! 实际执行的 SQL 提炼为同样的特征，两边做打分匹配。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 一条 SQL 发射点记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlRecord {
    /// 唯一 id：`Class#method` 或 XML `namespace.id`
    pub id: String,
    /// SQL 来源类别：annotation / xml / jdbc-template / mp-wrapper / mp-builtin
    pub kind: String,
    /// 提取出的 SQL 模板（MyBatis 动态标签已拍平为内部文本；未知值折为 `?`）
    pub sql: String,
    /// 归一化骨架 token 序列（字面量折叠为 ?，空白归一）
    pub skeleton: Vec<String>,
    /// 涉及的表（小写）
    pub tables: Vec<String>,
    /// 触碰的列（小写）
    pub columns: Vec<String>,
    /// 源码文件（相对项目根）
    pub file: String,
    /// 起始行号（1-based）
    pub line: u32,
    /// 所属类/接口的简单名
    pub owner: String,
    /// 方法名（XML 语句为 id 属性）
    pub method: String,
    /// MyBatis-Plus 内置方法合成的保守记录（SELECT * 等），匹配时降权
    pub mp_builtin: bool,
    /// 动态 SQL（含 <if>/<foreach> 或 Wrapper 链）标记：骨架只是形状近似
    pub dynamic: bool,
}

/// 调用图边：caller -> callee（`Class#method` 形式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
}

/// HTTP 路由入口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub controller: String,
    pub handler: String,
}

/// 实体 -> 表 映射。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityTable {
    pub entity: String,
    pub table: String,
    pub file: String,
}

/// 逆向索引条目：某个 `Class#method` 的直接调用者 + 可达路由。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseEntry {
    pub direct_callers: Vec<String>,
    pub routes: Vec<Route>,
}

/// 整张索引地图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub project: String,
    pub generated_at: String,
    pub records: Vec<SqlRecord>,
    pub call_graph: Vec<CallEdge>,
    pub routes: Vec<Route>,
    pub entities: Vec<EntityTable>,
    /// mapper/调用者 逆向索引（scan 时预计算）
    #[serde(default)]
    pub reverse: BTreeMap<String, ReverseEntry>,
}

impl Index {
    pub fn new(project: &str, generated_at: String) -> Self {
        Index {
            project: project.to_string(),
            generated_at,
            records: Vec::new(),
            call_graph: Vec::new(),
            routes: Vec::new(),
            entities: Vec::new(),
            reverse: BTreeMap::new(),
        }
    }
}
