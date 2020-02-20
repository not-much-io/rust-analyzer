//! This module contains an import search funcionality that is provided to the ra_assists module.
//! Later, this should be moved away to a separate crate that is accessible from the ra_assists module.

use hir::{db::HirDatabase, ModuleDef, SourceBinder};
use ra_prof::profile;
use ra_syntax::{ast, AstNode, SyntaxKind::NAME};

use crate::{
    defs::{classify_name, NameDefinition},
    symbol_index::{self, FileSymbol, Query},
    RootDatabase,
};

pub struct ImportsLocator<'a> {
    source_binder: SourceBinder<'a, RootDatabase>,
}

impl<'a> ImportsLocator<'a> {
    pub fn new(db: &'a RootDatabase) -> Self {
        Self { source_binder: SourceBinder::new(db) }
    }

    pub fn find_imports(&mut self, name_to_import: &str) -> Vec<ModuleDef> {
        let _p = profile("search_for_imports");
        let db = self.source_binder.db;

        let project_results = {
            let mut query = Query::new(name_to_import.to_string());
            query.exact();
            query.limit(40);
            symbol_index::world_symbols(db, query)
        };
        let lib_results = {
            let mut query = Query::new(name_to_import.to_string());
            query.libs();
            query.exact();
            query.limit(40);
            symbol_index::world_symbols(db, query)
        };

        project_results
            .into_iter()
            .chain(lib_results.into_iter())
            .filter_map(|import_candidate| self.get_name_definition(db, &import_candidate))
            .filter_map(|name_definition_to_import| match name_definition_to_import {
                NameDefinition::ModuleDef(module_def) => Some(module_def),
                _ => None,
            })
            .collect()
    }

    fn get_name_definition(
        &mut self,
        db: &impl HirDatabase,
        import_candidate: &FileSymbol,
    ) -> Option<NameDefinition> {
        let _p = profile("get_name_definition");
        let file_id = import_candidate.file_id.into();
        let candidate_node = import_candidate.ptr.to_node(&db.parse_or_expand(file_id)?);
        let candidate_name_node = if candidate_node.kind() != NAME {
            candidate_node.children().find(|it| it.kind() == NAME)?
        } else {
            candidate_node
        };
        classify_name(
            &mut self.source_binder,
            hir::InFile { file_id, value: &ast::Name::cast(candidate_name_node)? },
        )
    }
}
