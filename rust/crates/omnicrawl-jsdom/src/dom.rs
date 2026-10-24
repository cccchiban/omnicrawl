//! 轻量 DOM 快照：把抓到的 HTML 装进节点集，供沙箱脚本读写。
//!
//! 只实现正文提取需要的子集：元素树、属性、文本、`textContent` / `innerHTML` 读写，
//! 以及 `querySelector` 支持的选择器（标签、`#id`、`.class`、`[attr]`、`[attr=value]`、
//! 后代与逗号分组）。不做布局、样式级联、事件冒泡与表单行为——那些既不影响正文，
//! 也无法在无渲染引擎的前提下如实实现。

use scraper::{Html, Node as ScraperNode};

/// 元素对象上挂的节点号属性名：JS 侧只把它当不可见字段，读写一律走访问器。
pub(crate) const NODE_ID: &str = "__ornId";

const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

#[derive(Debug, Clone)]
pub(crate) enum Kind {
    Document,
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) parent: Option<usize>,
    pub(crate) children: Vec<usize>,
    pub(crate) kind: Kind,
}

pub(crate) struct Dom {
    pub(crate) nodes: Vec<Node>,
    pub(crate) root: usize,
    pub(crate) max_nodes: usize,
    pub(crate) truncated: bool,
}

impl Dom {
    pub(crate) fn from_html(html: &str, max_nodes: usize) -> Self {
        let mut dom = Self {
            nodes: vec![Node {
                parent: None,
                children: Vec::new(),
                kind: Kind::Document,
            }],
            root: 0,
            max_nodes,
            truncated: false,
        };
        let parsed = Html::parse_document(html);
        let root = parsed.tree.root();
        let children: Vec<_> = root.children().collect();
        for child in children {
            dom.convert(&parsed, 0, child);
        }
        if !dom.has_element("body") {
            // 缺 `<body>`（片段或畸形文档）时补一个，脚本普遍假定它存在。
            dom.push_element(0, "body");
        }
        dom
    }

    fn convert(&mut self, parsed: &Html, parent: usize, node: ego_tree::NodeRef<'_, ScraperNode>) {
        match node.value() {
            ScraperNode::Element(element) => {
                let attrs: Vec<(String, String)> = element
                    .attrs()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect();
                let Some(index) = self.push_element(parent, element.name()) else {
                    return;
                };
                if let Kind::Element { attrs: slot, .. } = &mut self.nodes[index].kind {
                    *slot = attrs;
                }
                let children: Vec<_> = node.children().collect();
                for child in children {
                    self.convert(parsed, index, child);
                }
            }
            ScraperNode::Text(text) => {
                self.push_text(parent, &text.text);
            }
            // 注释、doctype、处理指令对正文没有贡献：整枝跳过。
            _ => {}
        }
    }

    pub(crate) fn push_element(&mut self, parent: usize, tag: &str) -> Option<usize> {
        if self.nodes.len() >= self.max_nodes {
            self.truncated = true;
            return None;
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            parent: Some(parent),
            children: Vec::new(),
            kind: Kind::Element {
                tag: tag.to_ascii_lowercase(),
                attrs: Vec::new(),
            },
        });
        self.nodes[parent].children.push(index);
        Some(index)
    }

    pub(crate) fn push_text(&mut self, parent: usize, text: &str) -> Option<usize> {
        if text.is_empty() {
            return None;
        }
        if self.nodes.len() >= self.max_nodes {
            self.truncated = true;
            return None;
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            parent: Some(parent),
            children: Vec::new(),
            kind: Kind::Text(text.to_string()),
        });
        self.nodes[parent].children.push(index);
        Some(index)
    }

    pub(crate) fn tag(&self, index: usize) -> Option<&str> {
        match &self.nodes.get(index)?.kind {
            Kind::Element { tag, .. } => Some(tag),
            _ => None,
        }
    }

    /// 节点种类：1 元素、3 文本、9 文档（与 DOM 的 `nodeType` 一致）。
    pub(crate) fn node_type(&self, index: usize) -> u16 {
        match self.nodes.get(index).map(|node| &node.kind) {
            Some(Kind::Element { .. }) => 1,
            Some(Kind::Text(_)) => 3,
            Some(Kind::Document) => 9,
            None => 0,
        }
    }

    pub(crate) fn text_of(&self, index: usize) -> Option<&str> {
        match self.nodes.get(index).map(|node| &node.kind) {
            Some(Kind::Text(text)) => Some(text.as_str()),
            _ => None,
        }
    }

    pub(crate) fn children_of(&self, index: usize) -> &[usize] {
        self.nodes
            .get(index)
            .map(|node| node.children.as_slice())
            .unwrap_or(&[])
    }

    pub(crate) fn parent_of(&self, index: usize) -> Option<usize> {
        self.nodes.get(index).and_then(|node| node.parent)
    }

    /// 在 `parent` 下把 `child` 插到 `reference` 之前；`reference` 为空时等价于追加。
    ///
    /// 这是 `insertBefore` 的落点：React 的协调器只靠它做插入，没有它
    /// `appendChild` 单独存在也挂不上整棵树。
    pub(crate) fn insert_before(&mut self, parent: usize, child: usize, reference: Option<usize>) {
        if parent == child || self.is_ancestor(child, parent) {
            return;
        }
        if !matches!(
            self.nodes.get(parent).map(|node| &node.kind),
            Some(Kind::Element { .. }) | Some(Kind::Document)
        ) {
            return;
        }
        self.detach(child);
        let position = reference
            .and_then(|reference| {
                self.nodes[parent]
                    .children
                    .iter()
                    .position(|existing| *existing == reference)
            })
            .unwrap_or(self.nodes[parent].children.len());
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.insert(position, child);
    }

    /// 从父节点上摘掉 `child` 并返回它，供 `removeChild` 回传。
    pub(crate) fn remove_child(&mut self, parent: usize, child: usize) -> bool {
        let Some(node) = self.nodes.get_mut(parent) else {
            return false;
        };
        let before = node.children.len();
        node.children.retain(|existing| *existing != child);
        if node.children.len() == before {
            return false;
        }
        self.nodes[child].parent = None;
        true
    }

    /// 造一个游离节点：文本节点写文本，元素节点不挂任何属性。
    pub(crate) fn create_detached(&mut self, tag: Option<&str>, text: &str) -> Option<usize> {
        let index = match tag {
            Some(tag) => self.push_element(0, tag)?,
            None => {
                if text.is_empty() {
                    return None;
                }
                self.push_text(0, text)?
            }
        };
        self.detach(index);
        Some(index)
    }

    /// 深拷贝一棵子树（`cloneNode(true)`）；`deep` 为假时只复制节点自身。
    ///
    /// 副本必须是游离的：`<template>` 的 `content.cloneNode(true)` 就是这样
    /// 把内容搬进真实文档的，直接复用原节点会把它从模板里抢走。
    pub(crate) fn clone_tree(&mut self, index: usize, deep: bool) -> Option<usize> {
        let kind = self.nodes.get(index)?.kind.clone();
        let copy = match kind {
            Kind::Element { tag, attrs } => {
                let created = self.create_detached(Some(&tag), "")?;
                if let Kind::Element { attrs: slot, .. } = &mut self.nodes[created].kind {
                    *slot = attrs;
                }
                created
            }
            Kind::Text(text) => self.create_detached(None, &text)?,
            Kind::Document => return None,
        };
        if deep {
            let children = self.nodes[index].children.clone();
            for child in children {
                if let Some(cloned) = self.clone_tree(child, true) {
                    self.nodes[cloned].parent = Some(copy);
                    self.nodes[copy].children.push(cloned);
                }
            }
        }
        Some(copy)
    }

    /// 用新节点整体替换现有子节点（`replaceChildren`）。
    pub(crate) fn replace_children(&mut self, parent: usize, children: &[usize]) {
        let existing: Vec<usize> = self.nodes[parent].children.drain(..).collect();
        for child in existing {
            self.nodes[child].parent = None;
        }
        for child in children {
            self.insert_before(parent, *child, None);
        }
    }

    pub(crate) fn attr(&self, index: usize, name: &str) -> Option<&str> {
        match &self.nodes.get(index)?.kind {
            Kind::Element { attrs, .. } => attrs
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    pub(crate) fn set_attr(&mut self, index: usize, name: &str, value: &str) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if let Kind::Element { attrs, .. } = &mut node.kind {
            match attrs
                .iter_mut()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
            {
                Some(slot) => slot.1 = value.to_string(),
                None => attrs.push((name.to_string(), value.to_string())),
            }
        }
    }

    pub(crate) fn remove_attr(&mut self, index: usize, name: &str) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if let Kind::Element { attrs, .. } = &mut node.kind {
            attrs.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        }
    }

    pub(crate) fn text_content(&self, index: usize) -> String {
        let mut out = String::new();
        self.collect_text(index, &mut out);
        out
    }

    /// 不参与正文的元素：浏览器里它们的文本同样不进 `textContent` 聚合。
    fn is_non_text(tag: &str) -> bool {
        matches!(tag, "script" | "style" | "noscript" | "template")
    }

    fn collect_text(&self, index: usize, out: &mut String) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        match &node.kind {
            Kind::Text(text) => out.push_str(text),
            _ => {
                for child in &node.children {
                    // 从父级聚合时跳过脚本/样式：浏览器里它们的文本不进 `textContent`。
                    // 直接查询这些元素自身（作为 `index` 进来）仍能看到自己的文本。
                    if self.tag(*child).is_some_and(Self::is_non_text) {
                        continue;
                    }
                    self.collect_text(*child, out);
                }
            }
        }
    }

    /// 清空子节点后写入一段纯文本。
    pub(crate) fn set_text(&mut self, index: usize, text: &str) {
        let Some(node) = self.nodes.get_mut(index) else {
            return;
        };
        if matches!(node.kind, Kind::Element { .. }) {
            let children: Vec<usize> = node.children.drain(..).collect();
            for child in children {
                self.nodes[child].parent = None;
            }
        }
        self.push_text(index, text);
    }

    pub(crate) fn detach(&mut self, index: usize) {
        let Some(parent) = self.nodes.get(index).and_then(|node| node.parent) else {
            return;
        };
        self.nodes[parent].children.retain(|child| *child != index);
        self.nodes[index].parent = None;
    }

    pub(crate) fn append(&mut self, parent: usize, child: usize) {
        if parent == child || self.is_ancestor(child, parent) {
            return;
        }
        self.detach(child);
        if !matches!(self.nodes[parent].kind, Kind::Element { .. }) {
            return;
        }
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    fn is_ancestor(&self, candidate: usize, of: usize) -> bool {
        let mut current = self.nodes.get(of).and_then(|node| node.parent);
        while let Some(index) = current {
            if index == candidate {
                return true;
            }
            current = self.nodes.get(index).and_then(|node| node.parent);
        }
        false
    }

    pub(crate) fn first_element_named(&self, tag: &str) -> Option<usize> {
        (0..self.nodes.len()).find(|index| self.tag(*index).is_some_and(|name| name == tag))
    }

    fn has_element(&self, tag: &str) -> bool {
        self.first_element_named(tag).is_some()
    }

    /// 文档顺序（先序）的全部已挂载元素。
    pub(crate) fn elements(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_elements(self.root, &mut out);
        out
    }

    fn collect_elements(&self, index: usize, out: &mut Vec<usize>) {
        for child in &self.nodes[index].children {
            if self.tag(*child).is_some() {
                out.push(*child);
            }
            self.collect_elements(*child, out);
        }
    }

    /// 解析 HTML 片段并挂到 `parent` 下（`innerHTML` 与 `document.write` 共用）。
    pub(crate) fn insert_fragment(&mut self, parent: usize, html: &str) {
        let parsed = Html::parse_fragment(html);
        let root = parsed.tree.root();
        let children: Vec<_> = root.children().collect();
        for child in children {
            self.convert(&parsed, parent, child);
        }
    }

    pub(crate) fn inner_html(&self, index: usize) -> String {
        let mut out = String::new();
        for child in &self.nodes[index].children {
            self.serialize(*child, &mut out);
        }
        out
    }

    pub(crate) fn replace_children_with(&mut self, index: usize, html: &str) {
        let children: Vec<usize> = self.nodes[index].children.drain(..).collect();
        for child in children {
            self.nodes[child].parent = None;
        }
        self.insert_fragment(index, html);
    }

    pub(crate) fn serialize_document(&self) -> String {
        let mut out = String::new();
        for child in &self.nodes[self.root].children {
            self.serialize(*child, &mut out);
        }
        out
    }

    fn serialize(&self, index: usize, out: &mut String) {
        let Some(node) = self.nodes.get(index) else {
            return;
        };
        match &node.kind {
            Kind::Text(text) => escape_text(text, out),
            Kind::Document => {
                for child in &node.children {
                    self.serialize(*child, out);
                }
            }
            Kind::Element { tag, attrs } => {
                out.push('<');
                out.push_str(tag);
                for (name, value) in attrs {
                    out.push(' ');
                    out.push_str(name);
                    out.push_str("=\"");
                    escape_attr(value, out);
                    out.push('"');
                }
                out.push('>');
                if VOID_TAGS.contains(&tag.as_str()) {
                    return;
                }
                for child in &node.children {
                    self.serialize(*child, out);
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    /// `querySelector`：返回文档顺序的第一个命中。
    pub(crate) fn query_first(&self, scope: usize, selector: &str) -> Option<usize> {
        let groups = parse_selector(selector)?;
        self.query_all_parsed(scope, &groups).into_iter().next()
    }

    pub(crate) fn query_all(&self, scope: usize, selector: &str) -> Vec<usize> {
        match parse_selector(selector) {
            Some(groups) => self.query_all_parsed(scope, &groups),
            None => Vec::new(),
        }
    }

    fn query_all_parsed(&self, scope: usize, groups: &[Vec<Compound>]) -> Vec<usize> {
        self.elements()
            .into_iter()
            .filter(|candidate| {
                // 作用域节点自身不参与匹配（与 DOM 规范一致：只在后代里找）。
                *candidate != scope
                    && self.is_descendant_of(*candidate, scope)
                    && groups
                        .iter()
                        .any(|group| self.matches_group(*candidate, group, group.len()))
            })
            .collect()
    }

    fn is_descendant_of(&self, index: usize, ancestor: usize) -> bool {
        let mut current = self.nodes.get(index).and_then(|node| node.parent);
        while let Some(node) = current {
            if node == ancestor {
                return true;
            }
            current = self.nodes.get(node).and_then(|item| item.parent);
        }
        false
    }

    fn matches_group(&self, index: usize, group: &[Compound], last: usize) -> bool {
        if !self.matches_compound(index, &group[last - 1]) {
            return false;
        }
        if last == 1 {
            return true;
        }
        // 后代组合：剩下的复合选择器只要在祖先链上按序出现过即可。
        let mut remaining = last - 1;
        let mut current = self.nodes[index].parent;
        while let Some(node) = current {
            if remaining == 0 {
                return true;
            }
            if self.matches_compound(node, &group[remaining - 1]) {
                remaining -= 1;
                if remaining == 0 {
                    return true;
                }
            }
            current = self.nodes[node].parent;
        }
        false
    }

    fn matches_compound(&self, index: usize, compound: &Compound) -> bool {
        let Some(tag) = self.tag(index) else {
            return false;
        };
        if let Some(expected) = &compound.tag {
            if !expected.eq_ignore_ascii_case(tag) {
                return false;
            }
        }
        if let Some(id) = &compound.id {
            if self.attr(index, "id") != Some(id.as_str()) {
                return false;
            }
        }
        if !compound.classes.is_empty() {
            let classes = self.attr(index, "class").unwrap_or_default();
            let present: Vec<&str> = classes.split_whitespace().collect();
            if !compound
                .classes
                .iter()
                .all(|class| present.contains(&class.as_str()))
            {
                return false;
            }
        }
        for (name, expected) in &compound.attrs {
            match (self.attr(index, name), expected) {
                (None, _) => return false,
                (Some(actual), Some(value)) if actual != value.as_str() => return false,
                _ => {}
            }
        }
        true
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<(String, Option<String>)>,
}

/// 解析选择器：只认标签、`#id`、`.class`、`[attr]`、`[attr=value]`、`*`、后代与逗号分组。
///
/// 出现结构性伪类、`>`/`+`/`~`、`::` 等超出子集的语法时整条判为不支持并返回 `None`：
/// 与其猜一个近似结果，不如让脚本拿到空集，正文提取随后按原样回退。
pub(crate) fn parse_selector(selector: &str) -> Option<Vec<Vec<Compound>>> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains(['>', '+', '~', ':', ',']) && trimmed.contains(',') {
        // 逗号分组单独处理；先把分组切开再看每组是否合法。
    }
    let mut groups = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        let mut compounds = Vec::new();
        for token in part.split_whitespace() {
            compounds.push(parse_compound(token)?);
        }
        if compounds.is_empty() {
            return None;
        }
        groups.push(compounds);
    }
    Some(groups)
}

fn parse_compound(token: &str) -> Option<Compound> {
    if token.contains(['>', '+', '~', ':']) {
        return None;
    }
    let mut compound = Compound {
        tag: None,
        id: None,
        classes: Vec::new(),
        attrs: Vec::new(),
    };
    let mut rest = token;
    // 标签名（或 `*`）只能出现在最前面。
    let tag_len = rest
        .char_indices()
        .find(|(_, character)| *character == '#' || *character == '.' || *character == '[')
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    if tag_len > 0 {
        let tag = &rest[..tag_len];
        if tag != "*" {
            if !tag
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
            {
                return None;
            }
            compound.tag = Some(tag.to_string());
        }
        rest = &rest[tag_len..];
    }
    while !rest.is_empty() {
        let mut characters = rest.chars();
        match characters.next()? {
            '#' => {
                let end = text_run_len(&rest[1..]);
                if end == 0 {
                    return None;
                }
                compound.id = Some(rest[1..1 + end].to_string());
                rest = &rest[1 + end..];
            }
            '.' => {
                let end = text_run_len(&rest[1..]);
                if end == 0 {
                    return None;
                }
                compound.classes.push(rest[1..1 + end].to_string());
                rest = &rest[1 + end..];
            }
            '[' => {
                let close = rest.find(']')?;
                let inner = rest[1..close].trim();
                let (name, value) = match inner.split_once('=') {
                    Some((name, value)) => (
                        name.trim(),
                        Some(
                            value
                                .trim()
                                .trim_matches(|character| character == '"' || character == '\'')
                                .to_string(),
                        ),
                    ),
                    None => (inner, None),
                };
                if name.is_empty() {
                    return None;
                }
                compound.attrs.push((name.to_ascii_lowercase(), value));
                rest = &rest[close + 1..];
            }
            _ => return None,
        }
    }
    Some(compound)
}

fn text_run_len(text: &str) -> usize {
    text.char_indices()
        .find(|(_, character)| {
            !(character.is_ascii_alphanumeric() || *character == '-' || *character == '_')
        })
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

fn escape_text(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

fn escape_attr(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            other => out.push(other),
        }
    }
}
