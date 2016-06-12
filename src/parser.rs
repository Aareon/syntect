use syntax_definition::*;
use scope::*;
use onig::{self, Region};
use std::usize;
use std::i32;

#[derive(Debug, Clone)]
pub struct ParseState {
    stack: Vec<StateLevel>,
    first_line: bool,
}

#[derive(Debug, Clone)]
struct StateLevel {
    context: ContextPtr,
    prototype: Option<ContextPtr>,
    captures: Option<(Region, String)>,
}

#[derive(Debug)]
struct RegexMatch {
    regions: Region,
    context: ContextPtr,
    pat_index: usize,
}

// TODO cache actual matching regions
type MatchCache = Vec<bool>;

impl ParseState {
    pub fn new(syntax: &SyntaxDefinition) -> ParseState {
        let start_state = StateLevel {
            context: syntax.contexts["main"].clone(),
            prototype: None,
            captures: None,
        };
        ParseState {
            stack: vec![start_state],
            first_line: true,
        }
    }

    pub fn parse_line(&mut self, line: &str) -> Vec<(usize, ScopeStackOp)> {
        assert!(self.stack.len() > 0,
                "Somehow main context was popped from the stack");
        let mut match_start = 0;
        let mut res = Vec::new();
        if self.first_line {
            let cur_level = &self.stack[self.stack.len() - 1];
            let context = cur_level.context.borrow();
            if !context.meta_content_scope.is_empty() {
                res.push((0, ScopeStackOp::Push(context.meta_content_scope[0])));
            }
            self.first_line = false;
        }

        let mut regions = Region::with_capacity(8);
        let mut match_cache: MatchCache = Vec::with_capacity(64); // TODO find best capacity
        while self.parse_next_token(line,
                                    &mut match_start,
                                    &mut match_cache,
                                    &mut regions,
                                    &mut res) {
        }
        return res;
    }

    fn parse_next_token(&mut self,
                        line: &str,
                        start: &mut usize,
                        cache: &mut MatchCache,
                        regions: &mut Region,
                        ops: &mut Vec<(usize, ScopeStackOp)>)
                        -> bool {
        let cur_match = {
            let cur_level = &self.stack[self.stack.len() - 1];
            let mut min_start = usize::MAX;
            let mut cur_match: Option<RegexMatch> = None;
            let context_chain = self.stack
                .iter()
                .filter_map(|lvl| lvl.prototype.as_ref().map(|x| x.clone()))
                .chain(Some(cur_level.context.clone()).into_iter());
            println!("ptoken");
            let mut overall_index = 0;
            if cache.is_empty() {
                println!("freshcachetoken");
            }
            for ctx in context_chain {
                for (pat_context_ptr, pat_index) in context_iter(ctx) {
                    if overall_index < cache.len() && cache[overall_index] == false {
                        overall_index += 1;
                        continue; // we've determined this pattern doesn't match this line anywhere
                    }
                    if overall_index < cache.len() {
                        println!("cmiss");
                    }
                    let mut pat_context = pat_context_ptr.borrow_mut();
                    let mut match_pat = pat_context.match_at_mut(pat_index);

                    // println!("{:?}", match_pat.regex_str);
                    match_pat.ensure_compiled_if_possible();
                    let refs_regex = if cur_level.captures.is_some() && match_pat.has_captures {
                        let &(ref region, ref s) = cur_level.captures.as_ref().unwrap();
                        Some(match_pat.compile_with_refs(region, s))
                    } else {
                        None
                    };
                    let regex = if let Some(ref rgx) = refs_regex {
                        rgx
                    } else {
                        match_pat.regex.as_ref().unwrap()
                    };
                    println!("regsearch");
                    let matched = regex.search_with_options(line,
                                                            *start,
                                                            line.len(),
                                                            onig::SEARCH_OPTION_NONE,
                                                            Some(regions));
                    if overall_index >= cache.len() {
                        cache.push(matched.is_some());
                    } // TODO update the cache even if this is another time over
                    if let Some(match_start) = matched {
                        let match_end = regions.pos(0).unwrap().1;
                        // this is necessary to avoid infinite looping on dumb patterns
                        let does_something = match match_pat.operation {
                            MatchOperation::None => match_start != match_end,
                            _ => true,
                        };
                        if match_start < min_start && does_something {
                            min_start = match_start;
                            cur_match = Some(RegexMatch {
                                regions: regions.clone(),
                                context: pat_context_ptr.clone(),
                                pat_index: pat_index,
                            });
                        }
                    }

                    overall_index += 1;
                }
            }
            cur_match
        };

        if let Some(reg_match) = cur_match {
            let (_, match_end) = reg_match.regions.pos(0).unwrap();
            *start = match_end;
            let level_context = self.stack[self.stack.len() - 1].context.clone();
            let stack_changed = self.exec_pattern(line, reg_match, level_context, ops);
            if stack_changed {
                cache.clear();
                println!("cclear");
            }
            true
        } else {
            false
        }
    }

    /// Returns true if the stack was changed
    fn exec_pattern(&mut self,
                    line: &str,
                    reg_match: RegexMatch,
                    level_context_ptr: ContextPtr,
                    ops: &mut Vec<(usize, ScopeStackOp)>)
                    -> bool {
        let (match_start, match_end) = reg_match.regions.pos(0).unwrap();
        let context = reg_match.context.borrow();
        let pat = context.match_at(reg_match.pat_index);
        let level_context = level_context_ptr.borrow();
        // println!("running pattern {:?}", pat);

        self.push_meta_ops(true, match_start, &*level_context, &pat.operation, ops);
        for s in pat.scope.iter() {
            // println!("pushing {:?} at {}", s, match_start);
            ops.push((match_start, ScopeStackOp::Push(s.clone())));
        }
        if let Some(ref capture_map) = pat.captures {
            // captures could appear in an arbitrary order, have to produce ops in right order
            // ex: ((bob)|(hi))* could match hibob in wrong order, and outer has to push first
            // we don't have to handle a capture matching multiple times, Sublime doesn't
            let mut map: Vec<((usize, i32), ScopeStackOp)> = Vec::new();
            for (cap_index, scopes) in capture_map.iter() {
                if let Some((cap_start, cap_end)) = reg_match.regions.pos(*cap_index) {
                    for scope in scopes.iter() {
                        map.push(((cap_start, -((cap_end - cap_start) as i32)),
                                  ScopeStackOp::Push(scope.clone())));
                    }
                    map.push(((cap_end, i32::MIN), ScopeStackOp::Pop(scopes.len())));
                }
            }
            map.sort_by(|a, b| a.0.cmp(&b.0));
            for ((index, _), op) in map.into_iter() {
                ops.push((index, op));
            }
        }
        if !pat.scope.is_empty() {
            ops.push((match_end, ScopeStackOp::Pop(pat.scope.len())));
        }
        self.push_meta_ops(false, match_end, &*level_context, &pat.operation, ops);

        self.perform_op(line, &reg_match.regions, pat)
    }

    fn push_meta_ops(&self,
                     initial: bool,
                     index: usize,
                     cur_context: &Context,
                     match_op: &MatchOperation,
                     ops: &mut Vec<(usize, ScopeStackOp)>) {
        let involves_pop = match match_op {
            &MatchOperation::Pop => true,
            &MatchOperation::Set(_) => true,
            &MatchOperation::Push(_) => false,
            &MatchOperation::None => false,
        };
        // println!("metas ops for {:?}, is pop: {}, initial: {}",
        //          match_op,
        //          involves_pop,
        //          initial);
        // println!("{:?}", cur_context.meta_scope);
        if involves_pop {
            let v = if initial {
                &cur_context.meta_content_scope
            } else {
                &cur_context.meta_scope
            };
            if !v.is_empty() {
                ops.push((index, ScopeStackOp::Pop(v.len())));
            }
        }
        match match_op {
            &MatchOperation::Push(ref context_refs) |
            &MatchOperation::Set(ref context_refs) => {
                for r in context_refs {
                    let ctx_ptr = r.resolve();
                    let ctx = ctx_ptr.borrow();
                    let v = if initial {
                        &ctx.meta_scope
                    } else {
                        &ctx.meta_content_scope
                    };
                    for scope in v.iter() {
                        ops.push((index, ScopeStackOp::Push(scope.clone())));
                    }
                }
            }
            &MatchOperation::None |
            &MatchOperation::Pop => (),
        }
    }

    /// Returns true if the stack was changed
    fn perform_op(&mut self, line: &str, regions: &Region, pat: &MatchPattern) -> bool {
        let ctx_refs = match pat.operation {
            MatchOperation::Push(ref ctx_refs) => ctx_refs,
            MatchOperation::Set(ref ctx_refs) => {
                self.stack.pop();
                ctx_refs
            }
            MatchOperation::Pop => {
                self.stack.pop();
                return true;
            }
            MatchOperation::None => return false,
        };
        for (i, r) in ctx_refs.iter().enumerate() {
            let proto = if i == 0 {
                pat.with_prototype.clone()
            } else {
                None
            };
            let ctx_ptr = r.resolve();
            let captures = {
                let ctx = ctx_ptr.borrow();
                if ctx.uses_backrefs {
                    Some((regions.clone(), line.to_owned()))
                } else {
                    None
                }
            };
            self.stack.push(StateLevel {
                context: ctx_ptr,
                prototype: proto,
                captures: captures,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use package_set::PackageSet;
    use parser::*;
    use scope::*;
    use util::debug_print_ops;

    #[test]
    fn can_parse() {
        use scope::ScopeStackOp::{Push, Pop};
        let ps = PackageSet::load_from_folder("testdata/Packages").unwrap();
        let mut state = {
            let syntax = ps.find_syntax_by_name("Ruby on Rails").unwrap();
            ParseState::new(syntax)
        };
        let mut state2 = {
            let syntax = ps.find_syntax_by_name("HTML (Rails)").unwrap();
            ParseState::new(syntax)
        };

        let line = "module Bob::Wow::Troll::Five; 5; end";
        let ops = state.parse_line(line);
        debug_print_ops(line, &ops);

        let test_ops = vec![
            (0, Push(Scope::new("source.ruby.rails").unwrap())),
            (0, Push(Scope::new("meta.module.ruby").unwrap())),
            (0, Push(Scope::new("keyword.control.module.ruby").unwrap())),
            (6, Pop(1)),
            (7, Push(Scope::new("entity.name.type.module.ruby").unwrap())),
            (7, Push(Scope::new("entity.other.inherited-class.module.first.ruby").unwrap())),
            (10, Push(Scope::new("punctuation.separator.inheritance.ruby").unwrap())),
            (12, Pop(1)),
            (12, Pop(1)),
        ];
        assert_eq!(&ops[0..test_ops.len()], &test_ops[..]);

        let line2 = "def lol(wow = 5)";
        let ops2 = state.parse_line(line2);
        debug_print_ops(line2, &ops2);
        let test_ops2 =
            vec![(0, Push(Scope::new("meta.function.method.with-arguments.ruby").unwrap())),
                 (0, Push(Scope::new("keyword.control.def.ruby").unwrap())),
                 (3, Pop(1)),
                 (4, Push(Scope::new("entity.name.function.ruby").unwrap())),
                 (7, Pop(1)),
                 (7, Push(Scope::new("punctuation.definition.parameters.ruby").unwrap())),
                 (8, Pop(1)),
                 (8, Push(Scope::new("variable.parameter.function.ruby").unwrap())),
                 (12, Push(Scope::new("keyword.operator.assignment.ruby").unwrap())),
                 (13, Pop(1)),
                 (14, Push(Scope::new("constant.numeric.ruby").unwrap())),
                 (15, Pop(1)),
                 (15, Pop(1)),
                 (15, Push(Scope::new("punctuation.definition.parameters.ruby").unwrap())),
                 (16, Pop(1)),
                 (16, Pop(1))];
        assert_eq!(ops2, test_ops2);

        let line3 = "<script>var lol = '<% def wow(";
        let ops3 = state2.parse_line(line3);
        debug_print_ops(line3, &ops3);
        let mut test_stack = ScopeStack::new();
        test_stack.push(Scope::new("text.html.ruby").unwrap());
        test_stack.push(Scope::new("text.html.basic").unwrap());
        test_stack.push(Scope::new("source.js.embedded.html").unwrap());
        test_stack.push(Scope::new("string.quoted.single.js").unwrap());
        test_stack.push(Scope::new("source.ruby.rails.embedded.html").unwrap());
        test_stack.push(Scope::new("meta.function.method.with-arguments.ruby").unwrap());
        test_stack.push(Scope::new("variable.parameter.function.ruby").unwrap());
        let mut test_stack2 = ScopeStack::new();
        for &(_, ref op) in ops3.iter() {
            test_stack2.apply(op);
        }
        assert_eq!(test_stack2, test_stack);

        // for testing backrefs
        let line4 = "lol = <<-END wow END";
        let ops4 = state.parse_line(line4);
        debug_print_ops(line4, &ops4);
        let test_ops4 = vec![
            (4, Push(Scope::new("keyword.operator.assignment.ruby").unwrap())),
            (5, Pop(1)),
            (6, Push(Scope::new("string.unquoted.heredoc.ruby").unwrap())),
            (6, Push(Scope::new("punctuation.definition.string.begin.ruby").unwrap())),
            (12, Pop(1)),
            (16, Push(Scope::new("punctuation.definition.string.end.ruby").unwrap())),
            (20, Pop(1)),
            (20, Pop(1)),
        ];
        assert_eq!(ops4, test_ops4);

        // assert!(false);
    }
}
