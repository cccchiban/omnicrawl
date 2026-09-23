//! Python `difflib.SequenceMatcher` 的等价子集（`isjunk=None`、`autojunk=False`）。
//!
//! `tool_diff` 的旁注行号 diff 直接消费 opcodes，两侧分段必须逐段一致，因此这里按
//! CPython 的 Ratcliff/Obershelp 算法（最长匹配块 + 递归队列 + 相邻块合并）实现，
//! 不引入第三方 diff crate。对照数据集见本文件单测。

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Equal,
    Delete,
    Insert,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpCode {
    pub tag: Tag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

/// 对映 `difflib.Match(i, j, size)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub i: usize,
    pub j: usize,
    pub size: usize,
}

pub struct SequenceMatcher<'a> {
    a: &'a [String],
    b: &'a [String],
    b2j: HashMap<&'a str, Vec<usize>>,
    matching_blocks: Option<Vec<Match>>,
    opcodes: Option<Vec<OpCode>>,
}

impl<'a> SequenceMatcher<'a> {
    pub fn new(a: &'a [String], b: &'a [String]) -> Self {
        let mut b2j: HashMap<&'a str, Vec<usize>> = HashMap::new();
        for (index, element) in b.iter().enumerate() {
            b2j.entry(element.as_str()).or_default().push(index);
        }
        Self {
            a,
            b,
            b2j,
            matching_blocks: None,
            opcodes: None,
        }
    }

    /// 对映 `find_longest_match`（无 junk、无 popular 过滤）。
    fn find_longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> Match {
        let mut best = Match {
            i: alo,
            j: blo,
            size: 0,
        };
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            if let Some(indices) = self.b2j.get(self.a[i].as_str()) {
                for &j in indices {
                    if j < blo {
                        continue;
                    }
                    if j >= bhi {
                        break;
                    }
                    let k = j
                        .checked_sub(1)
                        .and_then(|previous| j2len.get(&previous))
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    newj2len.insert(j, k);
                    if k > best.size {
                        best.i = i + 1 - k;
                        best.j = j + 1 - k;
                        best.size = k;
                    }
                }
            }
            j2len = newj2len;
        }

        while best.i > alo && best.j > blo && self.a[best.i - 1] == self.b[best.j - 1] {
            best.i -= 1;
            best.j -= 1;
            best.size += 1;
        }
        while best.i + best.size < ahi
            && best.j + best.size < bhi
            && self.a[best.i + best.size] == self.b[best.j + best.size]
        {
            best.size += 1;
        }
        best
    }

    /// 对映 `get_matching_blocks`：末项是 `(len(a), len(b), 0)` 哨兵。
    pub fn get_matching_blocks(&mut self) -> Vec<Match> {
        if let Some(blocks) = &self.matching_blocks {
            return blocks.clone();
        }
        let la = self.a.len();
        let lb = self.b.len();
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut found: Vec<Match> = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let matched = self.find_longest_match(alo, ahi, blo, bhi);
            if matched.size > 0 {
                found.push(matched);
                if alo < matched.i && blo < matched.j {
                    queue.push((alo, matched.i, blo, matched.j));
                }
                if matched.i + matched.size < ahi && matched.j + matched.size < bhi {
                    queue.push((matched.i + matched.size, ahi, matched.j + matched.size, bhi));
                }
            }
        }
        found.sort_by_key(|block| (block.i, block.j, block.size));

        let mut blocks: Vec<Match> = Vec::new();
        let mut current = Match {
            i: 0,
            j: 0,
            size: 0,
        };
        for block in found {
            if current.i + current.size == block.i && current.j + current.size == block.j {
                current.size += block.size;
            } else {
                if current.size > 0 {
                    blocks.push(current);
                }
                current = block;
            }
        }
        if current.size > 0 {
            blocks.push(current);
        }
        blocks.push(Match {
            i: la,
            j: lb,
            size: 0,
        });
        self.matching_blocks = Some(blocks.clone());
        blocks
    }

    /// 对映 `get_opcodes`。
    pub fn get_opcodes(&mut self) -> Vec<OpCode> {
        if let Some(opcodes) = &self.opcodes {
            return opcodes.clone();
        }
        let mut answer: Vec<OpCode> = Vec::new();
        let mut i = 0usize;
        let mut j = 0usize;
        for block in self.get_matching_blocks() {
            let tag = if i < block.i && j < block.j {
                Some(Tag::Replace)
            } else if i < block.i {
                Some(Tag::Delete)
            } else if j < block.j {
                Some(Tag::Insert)
            } else {
                None
            };
            if let Some(tag) = tag {
                answer.push(OpCode {
                    tag,
                    i1: i,
                    i2: block.i,
                    j1: j,
                    j2: block.j,
                });
            }
            i = block.i + block.size;
            j = block.j + block.size;
            if block.size > 0 {
                answer.push(OpCode {
                    tag: Tag::Equal,
                    i1: block.i,
                    i2: i,
                    j1: block.j,
                    j2: j,
                });
            }
        }
        self.opcodes = Some(answer.clone());
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn opcodes(a: &[&str], b: &[&str]) -> Vec<(Tag, usize, usize, usize, usize)> {
        let a = lines(a);
        let b = lines(b);
        SequenceMatcher::new(&a, &b)
            .get_opcodes()
            .into_iter()
            .map(|op| (op.tag, op.i1, op.i2, op.j1, op.j2))
            .collect()
    }

    #[test]
    fn opcodes_match_cpython_dataset() {
        assert_eq!(
            opcodes(&["a", "b", "c", "d"], &["a", "x", "c", "d", "e"]),
            vec![
                (Tag::Equal, 0, 1, 0, 1),
                (Tag::Replace, 1, 2, 1, 2),
                (Tag::Equal, 2, 4, 2, 4),
                (Tag::Insert, 4, 4, 4, 5),
            ]
        );
        assert_eq!(
            opcodes(&["line1", "line2"], &["line1", "line2"]),
            vec![(Tag::Equal, 0, 2, 0, 2)]
        );
        assert_eq!(
            opcodes(&[], &["new1", "new2"]),
            vec![(Tag::Insert, 0, 0, 0, 2)]
        );
        assert_eq!(opcodes(&["gone"], &[]), vec![(Tag::Delete, 0, 1, 0, 0)]);
        assert_eq!(
            opcodes(&["a", "b", "c"], &["c", "b", "a"]),
            vec![
                (Tag::Insert, 0, 0, 0, 2),
                (Tag::Equal, 0, 1, 2, 3),
                (Tag::Delete, 1, 3, 3, 3),
            ]
        );
        assert_eq!(
            opcodes(&["1", "2", "3", "4", "5"], &["1", "2", "9", "4", "5"]),
            vec![
                (Tag::Equal, 0, 2, 0, 2),
                (Tag::Replace, 2, 3, 2, 3),
                (Tag::Equal, 3, 5, 3, 5),
            ]
        );
        assert_eq!(
            opcodes(&["x", "y"], &["x", "y", "z", "w"]),
            vec![(Tag::Equal, 0, 2, 0, 2), (Tag::Insert, 2, 2, 2, 4)]
        );
    }

    #[test]
    fn matching_blocks_end_with_sentinel() {
        let a = lines(&["a", "b", "c", "d"]);
        let b = lines(&["a", "x", "c", "d", "e"]);
        let mut matcher = SequenceMatcher::new(&a, &b);
        assert_eq!(
            matcher.get_matching_blocks(),
            vec![
                Match {
                    i: 0,
                    j: 0,
                    size: 1
                },
                Match {
                    i: 2,
                    j: 2,
                    size: 2
                },
                Match {
                    i: 4,
                    j: 5,
                    size: 0
                },
            ]
        );
        // 重复调用命中缓存，结果一致。
        assert_eq!(matcher.get_matching_blocks().len(), 3);
    }

    #[test]
    fn identical_inputs_are_one_equal_block() {
        let a = lines(&["same", "same", "same"]);
        let mut matcher = SequenceMatcher::new(&a, &a);
        assert_eq!(
            matcher.get_opcodes(),
            vec![OpCode {
                tag: Tag::Equal,
                i1: 0,
                i2: 3,
                j1: 0,
                j2: 3,
            }]
        );
    }

    #[test]
    fn repeated_lines_walk_one_step_at_a_time() {
        // 逐个追加的场景：每一步都是 insert，配合旁注行号推进。
        assert!(opcodes(&[], &[]).is_empty());
        let a = lines(&["1: a", "2: b"]);
        let b = lines(&["1: a", "2: b", "3: c"]);
        let mut matcher = SequenceMatcher::new(&a, &b);
        let ops = matcher.get_opcodes();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].tag, Tag::Equal);
        assert_eq!((ops[1].tag, ops[1].i1, ops[1].j1), (Tag::Insert, 2, 2));
    }
}
