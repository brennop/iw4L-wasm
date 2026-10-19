use bevy::platform::collections::HashMap;
use bevy::prelude::*;
use hud_iw4::{ExprError, ExprHost, Statement};

#[derive(Resource, Default)]
pub(crate) struct MenuExprCache {
    by_dump: HashMap<String, Result<Statement, ExprError>>,
    by_source: HashMap<asset_game::ExpressionId, Result<Statement, ExprError>>,
}

impl MenuExprCache {
    fn statement(&mut self, dump: &str) -> Result<&Statement, ExprError> {
        if !self.by_dump.contains_key(dump) {
            self.by_dump.insert(dump.to_owned(), Statement::parse(dump));
        }
        match self.by_dump.get(dump) {
            Some(Ok(statement)) => Ok(statement),
            Some(Err(err)) => Err(err.clone()),

            None => Err(ExprError::TruncatedDump),
        }
    }

    fn source_statement(
        &mut self,
        id: Option<asset_game::ExpressionId>,
        dump: &str,
    ) -> Result<&Statement, ExprError> {
        let Some(id) = id else {
            return self.statement(dump);
        };
        let parsed = self
            .by_source
            .entry(id)
            .or_insert_with(|| Statement::parse(dump));
        parsed.as_ref().map_err(Clone::clone)
    }

    pub(crate) fn is_true_source(
        &mut self,
        id: Option<asset_game::ExpressionId>,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<bool, ExprError> {
        self.source_statement(id, dump)?.is_true(host)
    }
    pub(crate) fn float_source(
        &mut self,
        id: Option<asset_game::ExpressionId>,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<f32, ExprError> {
        self.source_statement(id, dump)?.evaluate_float(host)
    }
    pub(crate) fn string_source(
        &mut self,
        id: Option<asset_game::ExpressionId>,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<String, ExprError> {
        self.source_statement(id, dump)?.evaluate_string(host)
    }

    pub(crate) fn is_true(&mut self, dump: &str, host: &impl ExprHost) -> Result<bool, ExprError> {
        self.statement(dump)?.is_true(host)
    }

    pub(crate) fn evaluate_float(
        &mut self,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<f32, ExprError> {
        self.statement(dump)?.evaluate_float(host)
    }

    pub(crate) fn evaluate_string(
        &mut self,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<String, ExprError> {
        self.statement(dump)?.evaluate_string(host)
    }

    pub(crate) fn evaluate(
        &mut self,
        dump: &str,
        host: &impl ExprHost,
    ) -> Result<hud_iw4::Operand, ExprError> {
        self.statement(dump)?.evaluate(host)
    }
}
