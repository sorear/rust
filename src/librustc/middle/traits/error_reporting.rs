// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::{
    FulfillmentError,
    FulfillmentErrorCode,
    MismatchedProjectionTypes,
    Obligation,
    ObligationCauseCode,
    OutputTypeParameterMismatch,
    TraitNotObjectSafe,
    PredicateObligation,
    SelectionError,
    ObjectSafetyViolation,
    MethodViolationCode,
    object_safety_violations,
};

use fmt_macros::{Parser, Piece, Position};
use middle::def_id::DefId;
use middle::infer::InferCtxt;
use middle::ty::{self, ToPredicate, HasTypeFlags, ToPolyTraitRef, TraitRef, Ty};
use middle::ty::fold::TypeFoldable;
use util::nodemap::{FnvHashMap, FnvHashSet};

use std::fmt;
use syntax::codemap::Span;
use syntax::attr::{AttributeMethods, AttrMetaMethods};

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TraitErrorKey<'tcx> {
    is_warning: bool,
    span: Span,
    predicate: ty::Predicate<'tcx>
}

impl<'tcx> TraitErrorKey<'tcx> {
    fn from_error<'a>(infcx: &InferCtxt<'a, 'tcx>,
                      e: &FulfillmentError<'tcx>) -> Self {
        let predicate =
            infcx.resolve_type_vars_if_possible(&e.obligation.predicate);
        TraitErrorKey {
            is_warning: is_warning(&e.obligation),
            span: e.obligation.cause.span,
            predicate: infcx.tcx.erase_regions(&predicate)
        }
    }
}

pub fn report_fulfillment_errors<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                           errors: &Vec<FulfillmentError<'tcx>>) {
    for error in errors {
        report_fulfillment_error(infcx, error);
    }
}

fn report_fulfillment_error<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                      error: &FulfillmentError<'tcx>) {
    let error_key = TraitErrorKey::from_error(infcx, error);
    debug!("report_fulfillment_errors({:?}) - key={:?}",
           error, error_key);
    if !infcx.reported_trait_errors.borrow_mut().insert(error_key) {
        debug!("report_fulfillment_errors: skipping duplicate");
        return;
    }
    match error.code {
        FulfillmentErrorCode::CodeSelectionError(ref e) => {
            report_selection_error(infcx, &error.obligation, e);
        }
        FulfillmentErrorCode::CodeProjectionError(ref e) => {
            report_projection_error(infcx, &error.obligation, e);
        }
        FulfillmentErrorCode::CodeAmbiguity => {
            maybe_report_ambiguity(infcx, &error.obligation);
        }
    }
}

fn is_warning<T>(obligation: &Obligation<T>) -> bool {
    obligation.cause.code.is_rfc1214()
}

pub fn report_projection_error<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                         obligation: &PredicateObligation<'tcx>,
                                         error: &MismatchedProjectionTypes<'tcx>)
{
    let predicate =
        infcx.resolve_type_vars_if_possible(&obligation.predicate);

    // The TyError created by normalize_to_error can end up being unified
    // into all obligations: for example, if our obligation is something
    // like `$X = <() as Foo<$X>>::Out` and () does not implement Foo<_>,
    // then $X will be unified with TyError, but the error still needs to be
    // reported.
    if !infcx.tcx.sess.has_errors() || !predicate.references_error() {
        span_err_or_warn!(
            is_warning(obligation), infcx.tcx.sess, obligation.cause.span, E0271,
            "type mismatch resolving `{}`: {}",
            predicate,
            error.err);
        note_obligation_cause(infcx, obligation);
    }
}

fn report_on_unimplemented<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                     trait_ref: &TraitRef<'tcx>,
                                     span: Span) -> Option<String> {
    let def_id = trait_ref.def_id;
    let mut report = None;
    for item in infcx.tcx.get_attrs(def_id).iter() {
        if item.check_name("rustc_on_unimplemented") {
            let err_sp = item.meta().span.substitute_dummy(span);
            let def = infcx.tcx.lookup_trait_def(def_id);
            let trait_str = def.trait_ref.to_string();
            if let Some(ref istring) = item.value_str() {
                let mut generic_map = def.generics.types.iter_enumerated()
                                         .map(|(param, i, gen)| {
                                               (gen.name.as_str().to_string(),
                                                trait_ref.substs.types.get(param, i)
                                                         .to_string())
                                              }).collect::<FnvHashMap<String, String>>();
                generic_map.insert("Self".to_string(),
                                   trait_ref.self_ty().to_string());
                let parser = Parser::new(&istring);
                let mut errored = false;
                let err: String = parser.filter_map(|p| {
                    match p {
                        Piece::String(s) => Some(s),
                        Piece::NextArgument(a) => match a.position {
                            Position::ArgumentNamed(s) => match generic_map.get(s) {
                                Some(val) => Some(val),
                                None => {
                                    span_err!(infcx.tcx.sess, err_sp, E0272,
                                                   "the #[rustc_on_unimplemented] \
                                                            attribute on \
                                                            trait definition for {} refers to \
                                                            non-existent type parameter {}",
                                                           trait_str, s);
                                    errored = true;
                                    None
                                }
                            },
                            _ => {
                                     span_err!(infcx.tcx.sess, err_sp, E0273,
                                               "the #[rustc_on_unimplemented] \
                                                        attribute on \
                                                        trait definition for {} must have named \
                                                        format arguments, \
                                                        eg `#[rustc_on_unimplemented = \
                                                        \"foo {{T}}\"]`",
                                                       trait_str);
                                errored = true;
                                None
                            }
                        }
                    }
                }).collect();
                // Report only if the format string checks out
                if !errored {
                    report = Some(err);
                }
            } else {
                span_err!(infcx.tcx.sess, err_sp, E0274,
                                        "the #[rustc_on_unimplemented] attribute on \
                                                 trait definition for {} must have a value, \
                                                 eg `#[rustc_on_unimplemented = \"foo\"]`",
                                                 trait_str);
            }
            break;
        }
    }
    report
}

/// Reports that an overflow has occurred and halts compilation. We
/// halt compilation unconditionally because it is important that
/// overflows never be masked -- they basically represent computations
/// whose result could not be truly determined and thus we can't say
/// if the program type checks or not -- and they are unusual
/// occurrences in any case.
pub fn report_overflow_error<'a, 'tcx, T>(infcx: &InferCtxt<'a, 'tcx>,
                                          obligation: &Obligation<'tcx, T>)
                                          -> !
    where T: fmt::Display + TypeFoldable<'tcx> + HasTypeFlags
{
    let predicate =
        infcx.resolve_type_vars_if_possible(&obligation.predicate);
    span_err!(infcx.tcx.sess, obligation.cause.span, E0275,
              "overflow evaluating the requirement `{}`",
              predicate);

    suggest_new_overflow_limit(infcx.tcx, obligation.cause.span);

    note_obligation_cause(infcx, obligation);

    infcx.tcx.sess.abort_if_errors();
    unreachable!();
}

pub fn report_selection_error<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                        obligation: &PredicateObligation<'tcx>,
                                        error: &SelectionError<'tcx>)
{
    let is_warning = is_warning(obligation);
    match *error {
        SelectionError::Unimplemented => {
            if let ObligationCauseCode::CompareImplMethodObligation = obligation.cause.code {
                span_err_or_warn!(
                    is_warning, infcx.tcx.sess, obligation.cause.span, E0276,
                    "the requirement `{}` appears on the impl \
                     method but not on the corresponding trait method",
                    obligation.predicate);
            } else {
                match obligation.predicate {
                    ty::Predicate::Trait(ref trait_predicate) => {
                        let trait_predicate =
                            infcx.resolve_type_vars_if_possible(trait_predicate);

                        if !infcx.tcx.sess.has_errors() || !trait_predicate.references_error() {
                            let trait_ref = trait_predicate.to_poly_trait_ref();
                            span_err_or_warn!(
                                is_warning, infcx.tcx.sess, obligation.cause.span, E0277,
                                "the trait `{}` is not implemented for the type `{}`",
                                trait_ref, trait_ref.self_ty());

                            // Check if it has a custom "#[rustc_on_unimplemented]"
                            // error message, report with that message if it does
                            let custom_note = report_on_unimplemented(infcx, &trait_ref.0,
                                                                      obligation.cause.span);
                            if let Some(s) = custom_note {
                                infcx.tcx.sess.fileline_note(obligation.cause.span, &s);
                            }
                            note_obligation_cause(infcx, obligation);
                        }
                    }

                    ty::Predicate::Equate(ref predicate) => {
                        let predicate = infcx.resolve_type_vars_if_possible(predicate);
                        let err = infcx.equality_predicate(obligation.cause.span,
                                                           &predicate).err().unwrap();
                        span_err_or_warn!(
                            is_warning, infcx.tcx.sess, obligation.cause.span, E0278,
                            "the requirement `{}` is not satisfied (`{}`)",
                            predicate,
                            err);
                        note_obligation_cause(infcx, obligation);
                    }

                    ty::Predicate::RegionOutlives(ref predicate) => {
                        let predicate = infcx.resolve_type_vars_if_possible(predicate);
                        let err = infcx.region_outlives_predicate(obligation.cause.span,
                                                                  &predicate).err().unwrap();
                        span_err_or_warn!(
                            is_warning, infcx.tcx.sess, obligation.cause.span, E0279,
                            "the requirement `{}` is not satisfied (`{}`)",
                            predicate,
                            err);
                        note_obligation_cause(infcx, obligation);
                    }

                    ty::Predicate::Projection(..) | ty::Predicate::TypeOutlives(..) => {
                        let predicate =
                            infcx.resolve_type_vars_if_possible(&obligation.predicate);
                        span_err_or_warn!(
                            is_warning, infcx.tcx.sess, obligation.cause.span, E0280,
                            "the requirement `{}` is not satisfied",
                            predicate);
                        note_obligation_cause(infcx, obligation);
                    }

                    ty::Predicate::ObjectSafe(trait_def_id) => {
                        let violations = object_safety_violations(
                            infcx.tcx, trait_def_id);
                        report_object_safety_error(infcx.tcx,
                                                   obligation.cause.span,
                                                   trait_def_id,
                                                   violations,
                                                   is_warning);
                        note_obligation_cause(infcx, obligation);
                    }

                    ty::Predicate::WellFormed(ty) => {
                        // WF predicates cannot themselves make
                        // errors. They can only block due to
                        // ambiguity; otherwise, they always
                        // degenerate into other obligations
                        // (which may fail).
                        infcx.tcx.sess.span_bug(
                            obligation.cause.span,
                            &format!("WF predicate not satisfied for {:?}", ty));
                    }
                }
            }
        }

        OutputTypeParameterMismatch(ref expected_trait_ref, ref actual_trait_ref, ref e) => {
            let expected_trait_ref = infcx.resolve_type_vars_if_possible(&*expected_trait_ref);
            let actual_trait_ref = infcx.resolve_type_vars_if_possible(&*actual_trait_ref);
            if !actual_trait_ref.self_ty().references_error() {
                span_err_or_warn!(
                    is_warning, infcx.tcx.sess, obligation.cause.span, E0281,
                    "type mismatch: the type `{}` implements the trait `{}`, \
                     but the trait `{}` is required ({})",
                    expected_trait_ref.self_ty(),
                    expected_trait_ref,
                    actual_trait_ref,
                    e);
                note_obligation_cause(infcx, obligation);
            }
        }

        TraitNotObjectSafe(did) => {
            let violations = object_safety_violations(infcx.tcx, did);
            report_object_safety_error(infcx.tcx, obligation.cause.span, did,
                                       violations, is_warning);
            note_obligation_cause(infcx, obligation);
        }
    }
}

pub fn report_object_safety_error<'tcx>(tcx: &ty::ctxt<'tcx>,
                                        span: Span,
                                        trait_def_id: DefId,
                                        violations: Vec<ObjectSafetyViolation>,
                                        is_warning: bool)
{
    span_err_or_warn!(
        is_warning, tcx.sess, span, E0038,
        "the trait `{}` cannot be made into an object",
        tcx.item_path_str(trait_def_id));

    let mut reported_violations = FnvHashSet();
    for violation in violations {
        if !reported_violations.insert(violation.clone()) {
            continue;
        }
        match violation {
            ObjectSafetyViolation::SizedSelf => {
                tcx.sess.fileline_note(
                    span,
                    "the trait cannot require that `Self : Sized`");
            }

            ObjectSafetyViolation::SupertraitSelf => {
                tcx.sess.fileline_note(
                    span,
                    "the trait cannot use `Self` as a type parameter \
                     in the supertrait listing");
            }

            ObjectSafetyViolation::Method(method,
                                          MethodViolationCode::StaticMethod) => {
                tcx.sess.fileline_note(
                    span,
                    &format!("method `{}` has no receiver",
                             method.name));
            }

            ObjectSafetyViolation::Method(method,
                                          MethodViolationCode::ReferencesSelf) => {
                tcx.sess.fileline_note(
                    span,
                    &format!("method `{}` references the `Self` type \
                              in its arguments or return type",
                             method.name));
            }

            ObjectSafetyViolation::Method(method,
                                          MethodViolationCode::Generic) => {
                tcx.sess.fileline_note(
                    span,
                    &format!("method `{}` has generic type parameters",
                             method.name));
            }
        }
    }
}

pub fn maybe_report_ambiguity<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                                        obligation: &PredicateObligation<'tcx>) {
    // Unable to successfully determine, probably means
    // insufficient type information, but could mean
    // ambiguous impls. The latter *ought* to be a
    // coherence violation, so we don't report it here.

    let predicate = infcx.resolve_type_vars_if_possible(&obligation.predicate);

    debug!("maybe_report_ambiguity(predicate={:?}, obligation={:?})",
           predicate,
           obligation);

    match predicate {
        ty::Predicate::Trait(ref data) => {
            let trait_ref = data.to_poly_trait_ref();
            let self_ty = trait_ref.self_ty();
            let all_types = &trait_ref.substs().types;
            if all_types.references_error() {
            } else if all_types.needs_infer() {
                // This is kind of a hack: it frequently happens that some earlier
                // error prevents types from being fully inferred, and then we get
                // a bunch of uninteresting errors saying something like "<generic
                // #0> doesn't implement Sized".  It may even be true that we
                // could just skip over all checks where the self-ty is an
                // inference variable, but I was afraid that there might be an
                // inference variable created, registered as an obligation, and
                // then never forced by writeback, and hence by skipping here we'd
                // be ignoring the fact that we don't KNOW the type works
                // out. Though even that would probably be harmless, given that
                // we're only talking about builtin traits, which are known to be
                // inhabited. But in any case I just threw in this check for
                // has_errors() to be sure that compilation isn't happening
                // anyway. In that case, why inundate the user.
                if !infcx.tcx.sess.has_errors() {
                    if
                        infcx.tcx.lang_items.sized_trait()
                        .map_or(false, |sized_id| sized_id == trait_ref.def_id())
                    {
                        need_type_info(infcx, obligation.cause.span, self_ty);
                    } else {
                        span_err!(infcx.tcx.sess, obligation.cause.span, E0283,
                                "type annotations required: cannot resolve `{}`",
                                predicate);
                        note_obligation_cause(infcx, obligation);
                    }
                }
            } else if !infcx.tcx.sess.has_errors() {
                // Ambiguity. Coherence should have reported an error.
                infcx.tcx.sess.span_bug(
                    obligation.cause.span,
                    &format!(
                        "coherence failed to report ambiguity: \
                         cannot locate the impl of the trait `{}` for \
                         the type `{}`",
                        trait_ref,
                        self_ty));
            }
        }

        ty::Predicate::WellFormed(ty) => {
            // Same hacky approach as above to avoid deluging user
            // with error messages.
            if !ty.references_error() && !infcx.tcx.sess.has_errors() {
                need_type_info(infcx, obligation.cause.span, ty);
            }
        }

        _ => {
            if !infcx.tcx.sess.has_errors() {
                span_err!(infcx.tcx.sess, obligation.cause.span, E0284,
                        "type annotations required: cannot resolve `{}`",
                        predicate);
                note_obligation_cause(infcx, obligation);
            }
        }
    }
}

fn need_type_info<'a, 'tcx>(infcx: &InferCtxt<'a, 'tcx>,
                            span: Span,
                            ty: Ty<'tcx>)
{
    span_err!(infcx.tcx.sess, span, E0282,
              "unable to infer enough type information about `{}`; \
               type annotations or generic parameter binding required",
              ty);
}

fn note_obligation_cause<'a, 'tcx, T>(infcx: &InferCtxt<'a, 'tcx>,
                                      obligation: &Obligation<'tcx, T>)
    where T: fmt::Display
{
    note_obligation_cause_code(infcx,
                               &obligation.predicate,
                               obligation.cause.span,
                               &obligation.cause.code);
}

fn note_obligation_cause_code<'a, 'tcx, T>(infcx: &InferCtxt<'a, 'tcx>,
                                           predicate: &T,
                                           cause_span: Span,
                                           cause_code: &ObligationCauseCode<'tcx>)
    where T: fmt::Display
{
    let tcx = infcx.tcx;
    match *cause_code {
        ObligationCauseCode::MiscObligation => { }
        ObligationCauseCode::RFC1214(ref subcode) => {
            tcx.sess.note_rfc_1214(cause_span);
            note_obligation_cause_code(infcx, predicate, cause_span, subcode);
        }
        ObligationCauseCode::SliceOrArrayElem => {
            tcx.sess.fileline_note(
                cause_span,
                &format!("slice and array elements must have `Sized` type"));
        }
        ObligationCauseCode::ProjectionWf(data) => {
            tcx.sess.fileline_note(
                cause_span,
                &format!("required so that the projection `{}` is well-formed",
                         data));
        }
        ObligationCauseCode::ReferenceOutlivesReferent(ref_ty) => {
            tcx.sess.fileline_note(
                cause_span,
                &format!("required so that reference `{}` does not outlive its referent",
                         ref_ty));
        }
        ObligationCauseCode::ItemObligation(item_def_id) => {
            let item_name = tcx.item_path_str(item_def_id);
            tcx.sess.fileline_note(
                cause_span,
                &format!("required by `{}`", item_name));
        }
        ObligationCauseCode::ObjectCastObligation(object_ty) => {
            tcx.sess.fileline_note(
                cause_span,
                &format!(
                    "required for the cast to the object type `{}`",
                    infcx.ty_to_string(object_ty)));
        }
        ObligationCauseCode::RepeatVec => {
            tcx.sess.fileline_note(
                cause_span,
                "the `Copy` trait is required because the \
                 repeated element will be copied");
        }
        ObligationCauseCode::VariableType(_) => {
            tcx.sess.fileline_note(
                cause_span,
                "all local variables must have a statically known size");
        }
        ObligationCauseCode::ReturnType => {
            tcx.sess.fileline_note(
                cause_span,
                "the return type of a function must have a \
                 statically known size");
        }
        ObligationCauseCode::AssignmentLhsSized => {
            tcx.sess.fileline_note(
                cause_span,
                "the left-hand-side of an assignment must have a statically known size");
        }
        ObligationCauseCode::StructInitializerSized => {
            tcx.sess.fileline_note(
                cause_span,
                "structs must have a statically known size to be initialized");
        }
        ObligationCauseCode::ClosureCapture(var_id, _, builtin_bound) => {
            let def_id = tcx.lang_items.from_builtin_kind(builtin_bound).unwrap();
            let trait_name = tcx.item_path_str(def_id);
            let name = tcx.local_var_name_str(var_id);
            tcx.sess.fileline_note(
                cause_span,
                &format!("the closure that captures `{}` requires that all captured variables \
                          implement the trait `{}`",
                         name,
                         trait_name));
        }
        ObligationCauseCode::FieldSized => {
            tcx.sess.fileline_note(
                cause_span,
                "only the last field of a struct or enum variant \
                 may have a dynamically sized type");
        }
        ObligationCauseCode::SharedStatic => {
            tcx.sess.fileline_note(
                cause_span,
                "shared static variables must have a type that implements `Sync`");
        }
        ObligationCauseCode::BuiltinDerivedObligation(ref data) => {
            let parent_trait_ref = infcx.resolve_type_vars_if_possible(&data.parent_trait_ref);
            tcx.sess.fileline_note(
                cause_span,
                &format!("required because it appears within the type `{}`",
                         parent_trait_ref.0.self_ty()));
            let parent_predicate = parent_trait_ref.to_predicate();
            note_obligation_cause_code(infcx, &parent_predicate, cause_span, &*data.parent_code);
        }
        ObligationCauseCode::ImplDerivedObligation(ref data) => {
            let parent_trait_ref = infcx.resolve_type_vars_if_possible(&data.parent_trait_ref);
            tcx.sess.fileline_note(
                cause_span,
                &format!("required because of the requirements on the impl of `{}` for `{}`",
                         parent_trait_ref,
                         parent_trait_ref.0.self_ty()));
            let parent_predicate = parent_trait_ref.to_predicate();
            note_obligation_cause_code(infcx, &parent_predicate, cause_span, &*data.parent_code);
        }
        ObligationCauseCode::CompareImplMethodObligation => {
            tcx.sess.fileline_note(
                cause_span,
                &format!("the requirement `{}` appears on the impl method \
                          but not on the corresponding trait method",
                         predicate));
        }
    }
}

fn suggest_new_overflow_limit(tcx: &ty::ctxt, span: Span) {
    let current_limit = tcx.sess.recursion_limit.get();
    let suggested_limit = current_limit * 2;
    tcx.sess.fileline_note(
        span,
        &format!(
            "consider adding a `#![recursion_limit=\"{}\"]` attribute to your crate",
            suggested_limit));
}
