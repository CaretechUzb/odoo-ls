use super::scope_map::ScopeMap;
use crate::core::symbols::storage::FileContentParent;
use thin_vec::ThinVec;

use lsp_types::Diagnostic;
use ruff_python_ast::{AtomicNodeIndex, ExprCall};
use ruff_text_size::{TextRange, TextSize};

use crate::{constants::{BuildStatus, BuildSteps, OYarn}, core::{evaluation::{Evaluation}, file_mgr::NoqaInfo, symbols::{SymbolTable, symbol_keys::{FunctionKey, SymbolKey, VariableKey, Wk}}}, oyarn, threads::SessionInfo};
use crate::core::evaluation_context::Context;

use super::{symbol_mgr::{SectionRange, SymbolMgr}};

#[derive(Debug, PartialEq, Clone)]
pub enum ArgumentType {
    #[allow(non_camel_case_types)]
    POS_ONLY,
    ARG,
    KWARG,
    VARARG,
    #[allow(non_camel_case_types)]
    KWORD_ONLY,
}

#[derive(Debug, Clone)]
pub struct Argument {
    pub symbol: Wk<VariableKey>,
    //other informations about arg
    /// Whether the parameter declares a default value (the value itself is never evaluated).
    pub has_default: bool,
    pub arg_type: ArgumentType,
    /// Range of the annotation in the file of the function declaring this parameter. Only hover
    /// reads it, by parsing that expression again (see `FeaturesUtils::argument_presentation`).
    pub annotation: Option<TextRange>,
}

#[derive(Debug)]
pub struct FunctionSymbol {
    pub name: OYarn,
    pub is_external: bool,
    pub is_static: bool,
    pub is_property: bool,
    pub doc_string: Option<crate::core::symbols::DocString>,
    pub node_index: AtomicNodeIndex,
    /// Diagnostics of the body per build step, collected by the validation of the parent like
    /// the ones stored on FileInfo. Only non-empty lists, by step: most functions have none, and
    /// an empty ThinVec is a pointer with no heap.
    diagnostics: ThinVec<(BuildSteps, Vec<Diagnostic>)>,
    pub evaluations: Vec<Evaluation>, //Vec, because sometimes a single allocation can be ambiguous, like ''' a = "5" if X else 5 '''
    pub(super) current_build_step: BuildSteps,
    pub(super) build_status: BuildStatus,
    pub range: TextRange,
    pub body_range: TextRange,
    pub args: Vec<Argument>,
    pub is_overloaded: bool, //used for @overload decorator. Only indicates if the decorator is present. Use is_overloaded() to know if this function is overloaded
    pub is_class_method: bool, //used for @classmethod decorator
    pub noqas: NoqaInfo,

    //Trait SymbolMgr
    pub sections: Vec<SectionRange>,

    // parent / child symbols
    parent: FileContentParent,
    //--- Body content
    pub(super) symbols: ScopeMap,
}

impl FunctionSymbol {

    pub fn new(name: &str, parent: FileContentParent, range: TextRange, body_start: TextSize, is_external: bool) -> Self {
        let mut res = Self {
            name: oyarn!("{}", name),
            is_external,
            parent,
            range,
            body_range: TextRange::new(body_start, range.end()),
            is_static: false,
            is_property: false,
            diagnostics: ThinVec::new(),
            node_index: AtomicNodeIndex::default(),
            doc_string: None,
            evaluations: vec![],
            current_build_step: BuildSteps::ARCH,
            build_status: BuildStatus::PENDING,
            sections: vec![],
            symbols: ScopeMap::default(),
            args: vec![],
            is_overloaded: false,
            is_class_method: false,
            noqas: NoqaInfo::None,
        };
        if name == "__new__" {
            res.is_static = true;
        }
        res._init_symbol_mgr();
        res
    }

    pub fn replace_diagnostics(&mut self, step: BuildSteps, diagnostics: Vec<Diagnostic>) {
        let at = self.diagnostics.partition_point(|(s, _)| (*s as u8) < (step as u8));
        let present = self.diagnostics.get(at).is_some_and(|(s, _)| *s == step);
        match (present, diagnostics.is_empty()) {
            (true, true) => { self.diagnostics.remove(at); },
            (true, false) => self.diagnostics[at].1 = diagnostics,
            (false, true) => {},
            (false, false) => self.diagnostics.insert(at, (step, diagnostics)),
        }
    }

    /// The diagnostics of every step, in step order.
    pub fn diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().flat_map(|(_, diagnostics)| diagnostics)
    }

    /*
    Add evaluations to possible return type of this function
     */
    pub fn add_return_evaluations(function: FunctionKey, session: &mut SessionInfo, evals: Vec<Evaluation>) {
        for new_eval in evals {
            let out_scope = new_eval.get_eval_out_of_function_scope(session, function);
            let function_symbol = &mut session.sync_odoo.symbol_table[function];
            for new_eval in out_scope {
                if !function_symbol.evaluations.contains(&new_eval) {
                    function_symbol.evaluations.push(new_eval);
                }
            }
        }
    }

    /// Return true if a previous implementation has the @overload decorator or has it itself
    pub fn is_func_overloaded(symbol_table: &SymbolTable, key: FunctionKey) -> bool {
        let func = &symbol_table[key];
        if func.is_overloaded {
            return true;
        }
        let previous_defs = symbol_table.get_content_symbol(func.parent().into(), &func.name, func.range.start().to_u32()).symbols;
        if let Some(&SymbolKey::Function(k)) = previous_defs.last() {
            return symbol_table[k].is_overloaded;
        }
        false
    }


    pub fn can_be_in_class(&self) -> bool {
        for arg in self.args.iter() {
            if arg.arg_type != ArgumentType::KWARG && arg.arg_type != ArgumentType::KWORD_ONLY {
                return true;
            }
        }
        false
    }

    /**
     * Given a specific context (with args, parent), adapt the evaluations of the function to get a more precise answer
     */
    pub fn get_return_type(&self, _session: &mut SessionInfo, _func_context: Option<Context>, _diagnostics: &mut Vec<Diagnostic>) -> Vec<Evaluation> {
        let res = vec![];
        /*for eval in self.evaluations.iter() {
            let mut new_eval = eval.clone();
            let symbol = new_eval.symbol.get_symbol(session, func_context.clone(), diagnostics);
            new_eval.symbol.symbol = symbol.0.clone();
            new_eval.symbol.instance = symbol.1;
            new_eval.symbol.get_symbol_hook = None;
            res.push(new_eval);
        }*/
        res
    }

    /* Given a call of this function and an index, return the corresponding parameter definition */
    pub fn get_indexed_arg_in_call<'a>(symbol_table: &'a SymbolTable, key: FunctionKey, call: &ExprCall, index: u32, is_on_instance: Option<bool>) -> Option<&'a Argument> {
        if Self::is_func_overloaded(symbol_table, key) {
            return None;
        }
        let func = &symbol_table[key];
        let mut call_arg_keyword = None;
        if index >= call.arguments.args.len() as u32 {
            call_arg_keyword = call.arguments.keywords.get((index - call.arguments.args.len() as u32) as usize);
        }
        let arg_index = if is_on_instance.unwrap_or(false) {
            index + 1
        } else {
            index
        };

        if let Some(keyword) = call_arg_keyword {
            for arg in func.args.iter() {
                if *symbol_table.name(arg.symbol.upgrade(symbol_table).unwrap()) == keyword.arg.as_ref().unwrap().id {
                    return Some(arg);
                }
            }
        } else {
            return func.args.get(arg_index as usize);
        }
        None
    }

    pub fn parent(&self) -> FileContentParent {
        self.parent
    }
}
