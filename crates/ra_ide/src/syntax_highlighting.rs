//! FIXME: write short doc here

use hir::{HirFileId, InFile, Name, SourceAnalyzer, SourceBinder};
use ra_db::SourceDatabase;
use ra_ide_db::{defs::NameDefinition, RootDatabase};
use ra_prof::profile;
use ra_syntax::{
    ast, AstNode, Direction, SyntaxElement, SyntaxKind, SyntaxKind::*, SyntaxToken, TextRange,
    WalkEvent, T,
};
use rustc_hash::FxHashMap;

use crate::{
    expand::descend_into_macros_with_analyzer,
    references::{classify_name, classify_name_ref},
    FileId,
};

pub mod tags {
    pub(crate) const FIELD: &str = "field";
    pub(crate) const FUNCTION: &str = "function";
    pub(crate) const MODULE: &str = "module";
    pub(crate) const CONSTANT: &str = "constant";
    pub(crate) const MACRO: &str = "macro";

    pub(crate) const VARIABLE: &str = "variable";
    pub(crate) const VARIABLE_MUT: &str = "variable.mut";

    pub(crate) const TYPE: &str = "type";
    pub(crate) const TYPE_BUILTIN: &str = "type.builtin";
    pub(crate) const TYPE_SELF: &str = "type.self";
    pub(crate) const TYPE_PARAM: &str = "type.param";
    pub(crate) const TYPE_LIFETIME: &str = "type.lifetime";

    pub(crate) const LITERAL_BYTE: &str = "literal.byte";
    pub(crate) const LITERAL_NUMERIC: &str = "literal.numeric";
    pub(crate) const LITERAL_CHAR: &str = "literal.char";

    pub(crate) const LITERAL_COMMENT: &str = "comment";
    pub(crate) const LITERAL_STRING: &str = "string";
    pub(crate) const LITERAL_ATTRIBUTE: &str = "attribute";

    pub(crate) const KEYWORD: &str = "keyword";
    pub(crate) const KEYWORD_UNSAFE: &str = "keyword.unsafe";
    pub(crate) const KEYWORD_CONTROL: &str = "keyword.control";
}

#[derive(Debug)]
pub struct HighlightedRange {
    pub range: TextRange,
    pub tag: &'static str,
    pub binding_hash: Option<u64>,
}

fn is_control_keyword(kind: SyntaxKind) -> bool {
    match kind {
        T![for]
        | T![loop]
        | T![while]
        | T![continue]
        | T![break]
        | T![if]
        | T![else]
        | T![match]
        | T![return] => true,
        _ => false,
    }
}

pub(crate) fn highlight(db: &RootDatabase, file_id: FileId) -> Vec<HighlightedRange> {
    let _p = profile("highlight");
    let parse = db.parse(file_id);
    let root = parse.tree().syntax().clone();

    let mut sb = SourceBinder::new(db);
    let mut bindings_shadow_count: FxHashMap<Name, u32> = FxHashMap::default();
    let mut res = Vec::new();
    let analyzer = sb.analyze(InFile::new(file_id.into(), &root), None);

    let mut in_macro_call = None;

    for event in root.preorder_with_tokens() {
        match event {
            WalkEvent::Enter(node) => match node.kind() {
                MACRO_CALL => {
                    in_macro_call = Some(node.clone());
                    if let Some(range) = highlight_macro(InFile::new(file_id.into(), node)) {
                        res.push(HighlightedRange { range, tag: tags::MACRO, binding_hash: None });
                    }
                }
                _ if in_macro_call.is_some() => {
                    if let Some(token) = node.as_token() {
                        if let Some((tag, binding_hash)) = highlight_token_tree(
                            &mut sb,
                            &analyzer,
                            &mut bindings_shadow_count,
                            InFile::new(file_id.into(), token.clone()),
                        ) {
                            res.push(HighlightedRange {
                                range: node.text_range(),
                                tag,
                                binding_hash,
                            });
                        }
                    }
                }
                _ => {
                    if let Some((tag, binding_hash)) = highlight_node(
                        &mut sb,
                        &mut bindings_shadow_count,
                        InFile::new(file_id.into(), node.clone()),
                    ) {
                        res.push(HighlightedRange { range: node.text_range(), tag, binding_hash });
                    }
                }
            },
            WalkEvent::Leave(node) => {
                if let Some(m) = in_macro_call.as_ref() {
                    if *m == node {
                        in_macro_call = None;
                    }
                }
            }
        }
    }

    res
}

fn highlight_macro(node: InFile<SyntaxElement>) -> Option<TextRange> {
    let macro_call = ast::MacroCall::cast(node.value.as_node()?.clone())?;
    let path = macro_call.path()?;
    let name_ref = path.segment()?.name_ref()?;

    let range_start = name_ref.syntax().text_range().start();
    let mut range_end = name_ref.syntax().text_range().end();
    for sibling in path.syntax().siblings_with_tokens(Direction::Next) {
        match sibling.kind() {
            T![!] | IDENT => range_end = sibling.text_range().end(),
            _ => (),
        }
    }

    Some(TextRange::from_to(range_start, range_end))
}

fn highlight_token_tree(
    sb: &mut SourceBinder<RootDatabase>,
    analyzer: &SourceAnalyzer,
    bindings_shadow_count: &mut FxHashMap<Name, u32>,
    token: InFile<SyntaxToken>,
) -> Option<(&'static str, Option<u64>)> {
    if token.value.parent().kind() != TOKEN_TREE {
        return None;
    }
    let token = descend_into_macros_with_analyzer(sb.db, analyzer, token);
    let expanded = {
        let parent = token.value.parent();
        // We only care Name and Name_ref
        match (token.value.kind(), parent.kind()) {
            (IDENT, NAME) | (IDENT, NAME_REF) => token.with_value(parent.into()),
            _ => token.map(|it| it.into()),
        }
    };

    highlight_node(sb, bindings_shadow_count, expanded)
}

fn highlight_node(
    sb: &mut SourceBinder<RootDatabase>,
    bindings_shadow_count: &mut FxHashMap<Name, u32>,
    node: InFile<SyntaxElement>,
) -> Option<(&'static str, Option<u64>)> {
    let db = sb.db;
    let mut binding_hash = None;
    let tag = match node.value.kind() {
        FN_DEF => {
            bindings_shadow_count.clear();
            return None;
        }
        COMMENT => tags::LITERAL_COMMENT,
        STRING | RAW_STRING | RAW_BYTE_STRING | BYTE_STRING => tags::LITERAL_STRING,
        ATTR => tags::LITERAL_ATTRIBUTE,
        // Special-case field init shorthand
        NAME_REF if node.value.parent().and_then(ast::RecordField::cast).is_some() => tags::FIELD,
        NAME_REF if node.value.ancestors().any(|it| it.kind() == ATTR) => return None,
        NAME_REF => {
            let name_ref = node.value.as_node().cloned().and_then(ast::NameRef::cast).unwrap();
            let name_kind = classify_name_ref(sb, node.with_value(&name_ref));
            match name_kind {
                Some(name_kind) => {
                    if let NameDefinition::Local(local) = &name_kind {
                        if let Some(name) = local.name(db) {
                            let shadow_count =
                                bindings_shadow_count.entry(name.clone()).or_default();
                            binding_hash =
                                Some(calc_binding_hash(node.file_id, &name, *shadow_count))
                        }
                    };

                    highlight_name(db, name_kind)
                }
                _ => return None,
            }
        }
        NAME => {
            let name = node.value.as_node().cloned().and_then(ast::Name::cast).unwrap();
            let name_kind = classify_name(sb, node.with_value(&name));

            if let Some(NameDefinition::Local(local)) = &name_kind {
                if let Some(name) = local.name(db) {
                    let shadow_count = bindings_shadow_count.entry(name.clone()).or_default();
                    *shadow_count += 1;
                    binding_hash = Some(calc_binding_hash(node.file_id, &name, *shadow_count))
                }
            };

            match name_kind {
                Some(name_kind) => highlight_name(db, name_kind),
                None => name.syntax().parent().map_or(tags::FUNCTION, |x| match x.kind() {
                    STRUCT_DEF | ENUM_DEF | TRAIT_DEF | TYPE_ALIAS_DEF => tags::TYPE,
                    TYPE_PARAM => tags::TYPE_PARAM,
                    RECORD_FIELD_DEF => tags::FIELD,
                    _ => tags::FUNCTION,
                }),
            }
        }
        INT_NUMBER | FLOAT_NUMBER => tags::LITERAL_NUMERIC,
        BYTE => tags::LITERAL_BYTE,
        CHAR => tags::LITERAL_CHAR,
        LIFETIME => tags::TYPE_LIFETIME,
        T![unsafe] => tags::KEYWORD_UNSAFE,
        k if is_control_keyword(k) => tags::KEYWORD_CONTROL,
        k if k.is_keyword() => tags::KEYWORD,

        _ => return None,
    };

    return Some((tag, binding_hash));

    fn calc_binding_hash(file_id: HirFileId, name: &Name, shadow_count: u32) -> u64 {
        fn hash<T: std::hash::Hash + std::fmt::Debug>(x: T) -> u64 {
            use std::{collections::hash_map::DefaultHasher, hash::Hasher};

            let mut hasher = DefaultHasher::new();
            x.hash(&mut hasher);
            hasher.finish()
        }

        hash((file_id, name, shadow_count))
    }
}

pub(crate) fn highlight_as_html(db: &RootDatabase, file_id: FileId, rainbow: bool) -> String {
    let parse = db.parse(file_id);

    fn rainbowify(seed: u64) -> String {
        use rand::prelude::*;
        let mut rng = SmallRng::seed_from_u64(seed);
        format!(
            "hsl({h},{s}%,{l}%)",
            h = rng.gen_range::<u16, _, _>(0, 361),
            s = rng.gen_range::<u16, _, _>(42, 99),
            l = rng.gen_range::<u16, _, _>(40, 91),
        )
    }

    let mut ranges = highlight(db, file_id);
    ranges.sort_by_key(|it| it.range.start());
    // quick non-optimal heuristic to intersect token ranges and highlighted ranges
    let mut frontier = 0;
    let mut could_intersect: Vec<&HighlightedRange> = Vec::new();

    let mut buf = String::new();
    buf.push_str(&STYLE);
    buf.push_str("<pre><code>");
    let tokens = parse.tree().syntax().descendants_with_tokens().filter_map(|it| it.into_token());
    for token in tokens {
        could_intersect.retain(|it| token.text_range().start() <= it.range.end());
        while let Some(r) = ranges.get(frontier) {
            if r.range.start() <= token.text_range().end() {
                could_intersect.push(r);
                frontier += 1;
            } else {
                break;
            }
        }
        let text = html_escape(&token.text());
        let ranges = could_intersect
            .iter()
            .filter(|it| token.text_range().is_subrange(&it.range))
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            buf.push_str(&text);
        } else {
            let classes = ranges.iter().map(|x| x.tag).collect::<Vec<_>>().join(" ");
            let binding_hash = ranges.first().and_then(|x| x.binding_hash);
            let color = match (rainbow, binding_hash) {
                (true, Some(hash)) => format!(
                    " data-binding-hash=\"{}\" style=\"color: {};\"",
                    hash,
                    rainbowify(hash)
                ),
                _ => "".into(),
            };
            buf.push_str(&format!("<span class=\"{}\"{}>{}</span>", classes, color, text));
        }
    }
    buf.push_str("</code></pre>");
    buf
}

fn highlight_name(db: &RootDatabase, def: NameDefinition) -> &'static str {
    match def {
        NameDefinition::Macro(_) => tags::MACRO,
        NameDefinition::StructField(_) => tags::FIELD,
        NameDefinition::ModuleDef(hir::ModuleDef::Module(_)) => tags::MODULE,
        NameDefinition::ModuleDef(hir::ModuleDef::Function(_)) => tags::FUNCTION,
        NameDefinition::ModuleDef(hir::ModuleDef::Adt(_)) => tags::TYPE,
        NameDefinition::ModuleDef(hir::ModuleDef::EnumVariant(_)) => tags::CONSTANT,
        NameDefinition::ModuleDef(hir::ModuleDef::Const(_)) => tags::CONSTANT,
        NameDefinition::ModuleDef(hir::ModuleDef::Static(_)) => tags::CONSTANT,
        NameDefinition::ModuleDef(hir::ModuleDef::Trait(_)) => tags::TYPE,
        NameDefinition::ModuleDef(hir::ModuleDef::TypeAlias(_)) => tags::TYPE,
        NameDefinition::ModuleDef(hir::ModuleDef::BuiltinType(_)) => tags::TYPE_BUILTIN,
        NameDefinition::SelfType(_) => tags::TYPE_SELF,
        NameDefinition::TypeParam(_) => tags::TYPE_PARAM,
        NameDefinition::Local(local) => {
            if local.is_mut(db) || local.ty(db).is_mutable_reference() {
                tags::VARIABLE_MUT
            } else {
                tags::VARIABLE
            }
        }
    }
}

//FIXME: like, real html escaping
fn html_escape(text: &str) -> String {
    text.replace("<", "&lt;").replace(">", "&gt;")
}

const STYLE: &str = "
<style>
body                { margin: 0; }
pre                 { color: #DCDCCC; background: #3F3F3F; font-size: 22px; padding: 0.4em; }

.comment            { color: #7F9F7F; }
.string             { color: #CC9393; }
.field              { color: #94BFF3; }
.function           { color: #93E0E3; }
.parameter          { color: #94BFF3; }
.text               { color: #DCDCCC; }
.type               { color: #7CB8BB; }
.type\\.builtin     { color: #8CD0D3; }
.type\\.param       { color: #20999D; }
.attribute          { color: #94BFF3; }
.literal            { color: #BFEBBF; }
.literal\\.numeric  { color: #6A8759; }
.macro              { color: #94BFF3; }
.module             { color: #AFD8AF; }
.variable           { color: #DCDCCC; }
.variable\\.mut     { color: #DCDCCC; text-decoration: underline; }

.keyword            { color: #F0DFAF; }
.keyword\\.unsafe   { color: #DFAF8F; }
.keyword\\.control  { color: #F0DFAF; font-weight: bold; }
</style>
";

#[cfg(test)]
mod tests {
    use std::fs;

    use test_utils::{assert_eq_text, project_dir, read_text};

    use crate::mock_analysis::{single_file, MockAnalysis};

    #[test]
    fn test_highlighting() {
        let (analysis, file_id) = single_file(
            r#"
#[derive(Clone, Debug)]
struct Foo {
    pub x: i32,
    pub y: i32,
}

fn foo<T>() -> T {
    unimplemented!();
    foo::<i32>();
}

macro_rules! def_fn {
    ($($tt:tt)*) => {$($tt)*}
}

def_fn!{
    fn bar() -> u32 {
        100
    }
}

// comment
fn main() {
    println!("Hello, {}!", 92);

    let mut vec = Vec::new();
    if true {
        let x = 92;
        vec.push(Foo { x, y: 1 });
    }
    unsafe { vec.set_len(0); }

    let mut x = 42;
    let y = &mut x;
    let z = &y;

    y;
}

enum E<X> {
    V(X)
}

impl<X> E<X> {
    fn new<T>() -> E<T> {}
}
"#
            .trim(),
        );
        let dst_file = project_dir().join("crates/ra_ide/src/snapshots/highlighting.html");
        let actual_html = &analysis.highlight_as_html(file_id, false).unwrap();
        let expected_html = &read_text(&dst_file);
        fs::write(dst_file, &actual_html).unwrap();
        assert_eq_text!(expected_html, actual_html);
    }

    #[test]
    fn test_rainbow_highlighting() {
        let (analysis, file_id) = single_file(
            r#"
fn main() {
    let hello = "hello";
    let x = hello.to_string();
    let y = hello.to_string();

    let x = "other color please!";
    let y = x.to_string();
}

fn bar() {
    let mut hello = "hello";
}
"#
            .trim(),
        );
        let dst_file = project_dir().join("crates/ra_ide/src/snapshots/rainbow_highlighting.html");
        let actual_html = &analysis.highlight_as_html(file_id, true).unwrap();
        let expected_html = &read_text(&dst_file);
        fs::write(dst_file, &actual_html).unwrap();
        assert_eq_text!(expected_html, actual_html);
    }

    #[test]
    fn accidentally_quadratic() {
        let file = project_dir().join("crates/ra_syntax/test_data/accidentally_quadratic");
        let src = fs::read_to_string(file).unwrap();

        let mut mock = MockAnalysis::new();
        let file_id = mock.add_file("/main.rs", &src);
        let host = mock.analysis_host();

        // let t = std::time::Instant::now();
        let _ = host.analysis().highlight(file_id).unwrap();
        // eprintln!("elapsed: {:?}", t.elapsed());
    }
}
