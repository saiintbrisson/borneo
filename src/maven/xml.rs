use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Deref, DerefMut},
    sync::LazyLock,
};

use anyhow::Context as _;

use quick_xml::{Reader, events::Event};
use regex::Regex;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub struct XmlFile {
    root: XmlNode,
}

impl XmlFile {
    pub fn from_str(xml: &str) -> anyhow::Result<Self> {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut buf = Vec::with_capacity(128);
        let pom = XmlNode::parse(&mut reader, &mut buf);

        let root = match pom {
            XmlNode::Text(_) => anyhow::bail!("expected root node"),
            XmlNode::Children(mut items) => items.pop().context("empty XML document")?.1,
            XmlNode::_Default => anyhow::bail!("empty XML document"),
        };

        Ok(Self { root })
    }

    pub fn merge_pom(&mut self, from: &XmlFile) {
        self.root.merge_node(&from.root);
    }

    pub fn replace_templates(&mut self, props: &BTreeMap<String, String>) {
        let mut props = props.clone();
        for (k, v) in java_system_properties() {
            props.entry(k).or_insert(v);
        }
        resolve_template_references(&mut props, &mut Default::default(), &self.root, &self.root);
        apply_templates(&props.clone(), &mut self.root);
    }
}

impl Deref for XmlFile {
    type Target = XmlNode;

    fn deref(&self) -> &Self::Target {
        &self.root
    }
}

impl DerefMut for XmlFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.root
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum XmlNode {
    Text(String),
    Children(Vec<(String, XmlNode)>),
    #[default]
    #[doc(hidden)]
    _Default,
}

impl XmlNode {
    fn find_str<'a>(&'a self, path: &str) -> Option<&'a str> {
        if let Some((lhs, rhs)) = path.split_once('.') {
            return self.get(lhs)?.find_str(rhs);
        } else if let Some(XmlNode::Text(val)) = self.get(path) {
            return Some(val);
        }

        None
    }

    pub fn get(&self, field: &str) -> Option<&XmlNode> {
        let Self::Children(children) = self else {
            return None;
        };

        let (lhs, rhs) = match field.split_once('/') {
            Some((lhs, rhs)) => (lhs, Some(rhs)),
            None => (field, None),
        };

        let child = children
            .iter()
            .find(|(name, _)| name == lhs)
            .map(|(_, val)| val)?;

        if let Some(rhs) = rhs {
            child.get(rhs)
        } else {
            Some(child)
        }
    }

    pub fn get_mut(&mut self, field: &str) -> Option<&mut XmlNode> {
        let Self::Children(children) = self else {
            return None;
        };

        let (lhs, rhs) = match field.split_once('/') {
            Some((lhs, rhs)) => (lhs, Some(rhs)),
            None => (field, None),
        };

        let child = children
            .iter_mut()
            .find(|(name, _)| name == lhs)
            .map(|(_, val)| val)?;

        if let Some(rhs) = rhs {
            child.get_mut(rhs)
        } else {
            Some(child)
        }
    }

    pub fn read_as<T: DeserializeOwned>(&self) -> de::Result<T> {
        T::deserialize(self)
    }

    fn parse(reader: &mut Reader<&[u8]>, buf: &mut Vec<u8>) -> Self {
        let mut children = Vec::new();
        let mut text = String::new();

        loop {
            match reader.read_event_into(buf) {
                Ok(Event::Start(e)) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    let child = Self::parse(reader, buf);
                    if !matches!(&child, XmlNode::Text(s) if s.trim().is_empty()) {
                        children.push((name, child));
                    }
                }
                Ok(Event::Text(e)) => {
                    text.push_str(std::str::from_utf8(&e).expect("XML text is not UTF-8"));
                }
                Ok(Event::End(_)) => break,
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }

        if children.is_empty() {
            Self::Text(text)
        } else {
            Self::Children(children)
        }
    }

    pub fn merge_node(&mut self, from: &XmlNode) {
        const FILTERED: &[&str] = &[
            "modelVersion",
            "artifactId",
            "packaging",
            "profiles",
            "prerequisites",
        ];

        const KEYED: &[(&str, &[&str])] = &[
            ("dependency", &["groupId", "artifactId"]),
            ("plugin", &["groupId", "artifactId"]),
            ("execution", &["id"]),
            ("reportSet", &["id"]),
            ("extension", &["groupId", "artifactId"]),
            ("exclusion", &["groupId", "artifactId"]),
        ];

        match (self, from) {
            (XmlNode::Children(a), XmlNode::Children(b)) if a != b => {
                let merging = b
                    .iter()
                    .filter(|ele| {
                        if FILTERED.contains(&&*ele.0) {
                            return false;
                        }
                        if conflicts_by_key(a, ele, KEYED) {
                            return false;
                        }
                        if let Some(child_entry) = a.iter_mut().find(|(tag, _)| tag == &ele.0) {
                            if KEYED.iter().any(|(k, _)| *k == ele.0) {
                                return true;
                            }

                            child_entry.1.merge_node(&ele.1);
                            return false;
                        }
                        true
                    })
                    .cloned()
                    .collect::<Vec<_>>();

                a.extend(merging);
            }
            _ => {}
        }
    }
}

fn conflicts_by_key(
    existing: &[(String, XmlNode)],
    candidate: &(String, XmlNode),
    keyed: &[(&str, &[&str])],
) -> bool {
    let Some(key_fields) = keyed
        .iter()
        .find(|(tag, _)| *tag == candidate.0)
        .map(|(_, keys)| keys)
    else {
        return false;
    };

    let Some(candidate_key) = extract_key(&candidate.1, key_fields) else {
        return false;
    };

    existing
        .iter()
        .filter(|(tag, _)| tag == &candidate.0)
        .any(|(_, node)| {
            extract_key(node, key_fields)
                .map(|k| k == candidate_key)
                .unwrap_or(false)
        })
}

fn extract_key<'a>(node: &'a XmlNode, key_fields: &[&str]) -> Option<Vec<&'a str>> {
    let XmlNode::Children(children) = node else {
        return None;
    };
    let keys: Vec<_> = key_fields
        .iter()
        .map(|field| {
            children
                .iter()
                .find(|(tag, _)| tag == field)
                .and_then(|(_, val)| match val {
                    XmlNode::Text(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect();
    Some(keys)
}

static TEMPLATE_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{(.+?)\}").unwrap());

fn resolve_template_references(
    props: &mut BTreeMap<String, String>,
    missing: &mut BTreeSet<String>,
    root: &XmlNode,
    node: &XmlNode,
) {
    match node {
        XmlNode::Text(v) => {
            for m in TEMPLATE_REGEX.find_iter(v.as_str()) {
                let trimmed = &m.as_str()[2..m.as_str().len() - 1];

                if props.contains_key(trimmed) {
                    continue;
                }

                let val = if let Some(node) = root.get("properties")
                    && let Some(XmlNode::Text(val)) = node.get(trimmed)
                {
                    Some(val.as_str())
                } else {
                    if let Some(("project", path)) = trimmed.split_once('.')
                        && let Some(val) = root.find_str(path)
                    {
                        Some(val)
                    } else {
                        None
                    }
                };

                let Some(val) = val else {
                    missing.insert(trimmed.to_string());
                    continue;
                };

                props.insert(trimmed.to_string(), val.to_string());
            }
        }
        XmlNode::Children(items) => {
            for (_, node) in items {
                resolve_template_references(props, missing, root, node);
            }
        }
        XmlNode::_Default => {}
    }
}

fn apply_templates(props: &BTreeMap<String, String>, node: &mut XmlNode) {
    match node {
        XmlNode::Text(v) => {
            let mut pos = 0;

            loop {
                if pos >= v.len() {
                    break;
                }
                let Some(m) = TEMPLATE_REGEX.find_at(v, pos) else {
                    break;
                };
                pos = m.end();
                let trimmed = &m.as_str()[2..m.as_str().len() - 1];
                if let Some(val) = props.get(trimmed) {
                    *v = v.replace(m.as_str(), val);
                }
            }
        }
        XmlNode::Children(items) => {
            for (_, node) in items {
                apply_templates(props, node);
            }
        }
        XmlNode::_Default => {}
    }
}

fn java_system_properties() -> BTreeMap<String, String> {
    let mut props = BTreeMap::new();

    let java_home = std::env::var("JAVA_HOME").unwrap_or_default();
    props.insert("java.home".into(), java_home.clone());

    if let Some(version) = crate::java::read_java_version(std::path::Path::new(&java_home)) {
        props.insert("java.version".into(), version);
    }

    props.insert("os.name".into(), std::env::consts::OS.into());
    props.insert("os.arch".into(), std::env::consts::ARCH.into());

    if let Some(home) = std::env::home_dir() {
        props.insert("user.home".into(), home.to_string_lossy().into_owned());
    }

    props.insert(
        "user.dir".into(),
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    props.insert(
        "file.separator".into(),
        std::path::MAIN_SEPARATOR.to_string(),
    );
    props.insert(
        "path.separator".into(),
        crate::java::PATH_SEPARATOR.to_string(),
    );

    props
}

mod de {
    use serde::{
        Deserializer,
        de::{DeserializeSeed, IntoDeserializer, Visitor},
    };

    use super::*;

    type Error = quick_xml::DeError;
    pub type Result<T> = std::result::Result<T, Error>;

    impl<'de> Deserializer<'de> for &XmlNode {
        type Error = Error;

        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            match self {
                XmlNode::Text(s) => visitor.visit_str(s),
                XmlNode::Children(c) => visitor.visit_map(MapAccess {
                    iter: c.iter(),
                    value: None,
                }),
                XmlNode::_Default => Err(Error::Custom("unexpected default node".to_string())),
            }
        }

        fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
            self.deserialize_any(v)
        }
        fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
            self.deserialize_any(v)
        }
        fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            visitor.visit_some(self)
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &str,
            _fields: &[&str],
            v: V,
        ) -> Result<V::Value> {
            self.deserialize_any(v)
        }

        fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            match self {
                XmlNode::Children(c) => visitor.visit_seq(SeqAccess { iter: c.iter() }),
                _ => Err(Error::Custom("expected a sequence".to_string())),
            }
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &str,
            _variants: &[&str],
            visitor: V,
        ) -> Result<V::Value> {
            match self {
                XmlNode::Text(s) => visitor.visit_enum(s.as_str().into_deserializer()),
                _ => Err(Error::Custom("expected string for enum".to_string())),
            }
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64
            char bytes byte_buf unit unit_struct newtype_struct
            tuple tuple_struct map identifier ignored_any
        }
    }

    struct MapAccess<'a> {
        iter: std::slice::Iter<'a, (String, XmlNode)>,
        value: Option<&'a XmlNode>,
    }

    impl<'de, 'a> serde::de::MapAccess<'de> for MapAccess<'a> {
        type Error = Error;

        fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
            match self.iter.next() {
                Some((key, val)) => {
                    self.value = Some(val);
                    seed.deserialize(key.as_str().into_deserializer()).map(Some)
                }
                None => Ok(None),
            }
        }

        fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
            seed.deserialize(self.value.take().expect("value already taken"))
        }
    }

    struct SeqAccess<'a> {
        iter: std::slice::Iter<'a, (String, XmlNode)>,
    }

    impl<'de, 'a> serde::de::SeqAccess<'de> for SeqAccess<'a> {
        type Error = Error;

        fn next_element_seed<T: DeserializeSeed<'de>>(
            &mut self,
            seed: T,
        ) -> Result<Option<T::Value>> {
            match self.iter.next() {
                Some((_, node)) => seed.deserialize(node).map(Some),
                None => Ok(None),
            }
        }
    }
}
