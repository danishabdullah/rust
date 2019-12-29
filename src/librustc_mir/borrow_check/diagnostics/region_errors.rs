//! Error reporting machinery for lifetime errors.

use rustc::infer::{
    error_reporting::nice_region_error::NiceRegionError, InferCtxt, NLLRegionVariableOrigin,
};
use rustc::mir::ConstraintCategory;
use rustc::ty::{self, RegionVid, Ty};
use rustc_errors::{Applicability, DiagnosticBuilder};
use rustc_hir::def_id::DefId;
use rustc_span::symbol::kw;
use rustc_span::Span;

use crate::util::borrowck_errors;

use crate::borrow_check::{
    nll::ConstraintDescription,
    region_infer::{values::RegionElement, RegionInferenceContext, TypeTest},
    universal_regions::DefiningTy,
    MirBorrowckCtxt,
};

use super::{OutlivesSuggestionBuilder, RegionErrorNamingCtx, RegionName, RegionNameSource};

impl ConstraintDescription for ConstraintCategory {
    fn description(&self) -> &'static str {
        // Must end with a space. Allows for empty names to be provided.
        match self {
            ConstraintCategory::Assignment => "assignment ",
            ConstraintCategory::Return => "returning this value ",
            ConstraintCategory::Yield => "yielding this value ",
            ConstraintCategory::UseAsConst => "using this value as a constant ",
            ConstraintCategory::UseAsStatic => "using this value as a static ",
            ConstraintCategory::Cast => "cast ",
            ConstraintCategory::CallArgument => "argument ",
            ConstraintCategory::TypeAnnotation => "type annotation ",
            ConstraintCategory::ClosureBounds => "closure body ",
            ConstraintCategory::SizedBound => "proving this value is `Sized` ",
            ConstraintCategory::CopyBound => "copying this value ",
            ConstraintCategory::OpaqueType => "opaque type ",
            ConstraintCategory::Boring
            | ConstraintCategory::BoringNoLocation
            | ConstraintCategory::Internal => "",
        }
    }
}

/// A collection of errors encountered during region inference. This is needed to efficiently
/// report errors after borrow checking.
///
/// Usually we expect this to either be empty or contain a small number of items, so we can avoid
/// allocation most of the time.
crate type RegionErrors<'tcx> = Vec<RegionErrorKind<'tcx>>;

#[derive(Clone, Debug)]
crate enum RegionErrorKind<'tcx> {
    /// A generic bound failure for a type test (`T: 'a`).
    TypeTestError { type_test: TypeTest<'tcx> },

    /// An unexpected hidden region for an opaque type.
    UnexpectedHiddenRegion {
        /// The def id of the opaque type.
        opaque_type_def_id: DefId,
        /// The hidden type.
        hidden_ty: Ty<'tcx>,
        /// The unexpected region.
        member_region: ty::Region<'tcx>,
    },

    /// Higher-ranked subtyping error.
    BoundUniversalRegionError {
        /// The placeholder free region.
        longer_fr: RegionVid,
        /// The region element that erroneously must be outlived by `longer_fr`.
        error_element: RegionElement,
        /// The origin of the placeholder region.
        fr_origin: NLLRegionVariableOrigin,
    },

    /// Any other lifetime error.
    RegionError {
        /// The origin of the region.
        fr_origin: NLLRegionVariableOrigin,
        /// The region that should outlive `shorter_fr`.
        longer_fr: RegionVid,
        /// The region that should be shorter, but we can't prove it.
        shorter_fr: RegionVid,
        /// Indicates whether this is a reported error. We currently only report the first error
        /// encountered and leave the rest unreported so as not to overwhelm the user.
        is_reported: bool,
    },
}

/// Information about the various region constraints involved in a borrow checker error.
#[derive(Clone, Debug)]
pub struct ErrorConstraintInfo {
    // fr: outlived_fr
    pub(super) fr: RegionVid,
    pub(super) fr_is_local: bool,
    pub(super) outlived_fr: RegionVid,
    pub(super) outlived_fr_is_local: bool,

    // Category and span for best blame constraint
    pub(super) category: ConstraintCategory,
    pub(super) span: Span,
}

impl<'tcx> RegionInferenceContext<'tcx> {
    /// Converts a region inference variable into a `ty::Region` that
    /// we can use for error reporting. If `r` is universally bound,
    /// then we use the name that we have on record for it. If `r` is
    /// existentially bound, then we check its inferred value and try
    /// to find a good name from that. Returns `None` if we can't find
    /// one (e.g., this is just some random part of the CFG).
    pub fn to_error_region(&self, r: RegionVid) -> Option<ty::Region<'tcx>> {
        self.to_error_region_vid(r).and_then(|r| self.definitions[r].external_name)
    }

    /// Returns the [RegionVid] corresponding to the region returned by
    /// `to_error_region`.
    pub fn to_error_region_vid(&self, r: RegionVid) -> Option<RegionVid> {
        if self.universal_regions.is_universal_region(r) {
            Some(r)
        } else {
            let r_scc = self.constraint_sccs.scc(r);
            let upper_bound = self.universal_upper_bound(r);
            if self.scc_values.contains(r_scc, upper_bound) {
                self.to_error_region_vid(upper_bound)
            } else {
                None
            }
        }
    }

    /// Returns `true` if a closure is inferred to be an `FnMut` closure.
    crate fn is_closure_fn_mut(&self, infcx: &InferCtxt<'_, 'tcx>, fr: RegionVid) -> bool {
        if let Some(ty::ReFree(free_region)) = self.to_error_region(fr) {
            if let ty::BoundRegion::BrEnv = free_region.bound_region {
                if let DefiningTy::Closure(def_id, substs) = self.universal_regions.defining_ty {
                    let closure_kind_ty = substs.as_closure().kind_ty(def_id, infcx.tcx);
                    return Some(ty::ClosureKind::FnMut) == closure_kind_ty.to_opt_closure_kind();
                }
            }
        }

        false
    }

    /// Report an error because the universal region `fr` was required to outlive
    /// `outlived_fr` but it is not known to do so. For example:
    ///
    /// ```
    /// fn foo<'a, 'b>(x: &'a u32) -> &'b u32 { x }
    /// ```
    ///
    /// Here we would be invoked with `fr = 'a` and `outlived_fr = `'b`.
    pub(in crate::borrow_check) fn report_error<'a>(
        &'a self,
        mbcx: &MirBorrowckCtxt<'a, 'tcx>,
        fr: RegionVid,
        fr_origin: NLLRegionVariableOrigin,
        outlived_fr: RegionVid,
        outlives_suggestion: &mut OutlivesSuggestionBuilder,
        renctx: &mut RegionErrorNamingCtx,
    ) -> DiagnosticBuilder<'a> {
        debug!("report_error(fr={:?}, outlived_fr={:?})", fr, outlived_fr);

        let (category, _, span) = self.best_blame_constraint(&mbcx.body, fr, fr_origin, |r| {
            self.provides_universal_region(r, fr, outlived_fr)
        });

        debug!("report_error: category={:?} {:?}", category, span);
        // Check if we can use one of the "nice region errors".
        if let (Some(f), Some(o)) = (self.to_error_region(fr), self.to_error_region(outlived_fr)) {
            let tables = mbcx.infcx.tcx.typeck_tables_of(mbcx.mir_def_id);
            let nice = NiceRegionError::new_from_span(mbcx.infcx, span, o, f, Some(tables));
            if let Some(diag) = nice.try_report_from_nll() {
                return diag;
            }
        }

        let (fr_is_local, outlived_fr_is_local): (bool, bool) = (
            self.universal_regions.is_local_free_region(fr),
            self.universal_regions.is_local_free_region(outlived_fr),
        );

        debug!(
            "report_error: fr_is_local={:?} outlived_fr_is_local={:?} category={:?}",
            fr_is_local, outlived_fr_is_local, category
        );

        let errci = ErrorConstraintInfo {
            fr,
            outlived_fr,
            fr_is_local,
            outlived_fr_is_local,
            category,
            span,
        };

        match (category, fr_is_local, outlived_fr_is_local) {
            (ConstraintCategory::Return, true, false) if self.is_closure_fn_mut(mbcx.infcx, fr) => {
                self.report_fnmut_error(mbcx, &errci, renctx)
            }
            (ConstraintCategory::Assignment, true, false)
            | (ConstraintCategory::CallArgument, true, false) => {
                let mut db = self.report_escaping_data_error(mbcx, &errci, renctx);

                outlives_suggestion.intermediate_suggestion(mbcx, &errci, renctx, &mut db);
                outlives_suggestion.collect_constraint(fr, outlived_fr);

                db
            }
            _ => {
                let mut db = self.report_general_error(mbcx, &errci, renctx);

                outlives_suggestion.intermediate_suggestion(mbcx, &errci, renctx, &mut db);
                outlives_suggestion.collect_constraint(fr, outlived_fr);

                db
            }
        }
    }

    /// Report a specialized error when `FnMut` closures return a reference to a captured variable.
    /// This function expects `fr` to be local and `outlived_fr` to not be local.
    ///
    /// ```text
    /// error: captured variable cannot escape `FnMut` closure body
    ///   --> $DIR/issue-53040.rs:15:8
    ///    |
    /// LL |     || &mut v;
    ///    |     -- ^^^^^^ creates a reference to a captured variable which escapes the closure body
    ///    |     |
    ///    |     inferred to be a `FnMut` closure
    ///    |
    ///    = note: `FnMut` closures only have access to their captured variables while they are
    ///            executing...
    ///    = note: ...therefore, returned references to captured variables will escape the closure
    /// ```
    fn report_fnmut_error(
        &self,
        mbcx: &MirBorrowckCtxt<'_, 'tcx>,
        errci: &ErrorConstraintInfo,
        renctx: &mut RegionErrorNamingCtx,
    ) -> DiagnosticBuilder<'_> {
        let ErrorConstraintInfo { outlived_fr, span, .. } = errci;

        let mut diag = mbcx
            .infcx
            .tcx
            .sess
            .struct_span_err(*span, "captured variable cannot escape `FnMut` closure body");

        // We should check if the return type of this closure is in fact a closure - in that
        // case, we can special case the error further.
        let return_type_is_closure = self.universal_regions.unnormalized_output_ty.is_closure();
        let message = if return_type_is_closure {
            "returns a closure that contains a reference to a captured variable, which then \
             escapes the closure body"
        } else {
            "returns a reference to a captured variable which escapes the closure body"
        };

        diag.span_label(*span, message);

        match self.give_region_a_name(mbcx, renctx, *outlived_fr).unwrap().source {
            RegionNameSource::NamedEarlyBoundRegion(fr_span)
            | RegionNameSource::NamedFreeRegion(fr_span)
            | RegionNameSource::SynthesizedFreeEnvRegion(fr_span, _)
            | RegionNameSource::CannotMatchHirTy(fr_span, _)
            | RegionNameSource::MatchedHirTy(fr_span)
            | RegionNameSource::MatchedAdtAndSegment(fr_span)
            | RegionNameSource::AnonRegionFromUpvar(fr_span, _)
            | RegionNameSource::AnonRegionFromOutput(fr_span, _, _) => {
                diag.span_label(fr_span, "inferred to be a `FnMut` closure");
            }
            _ => {}
        }

        diag.note(
            "`FnMut` closures only have access to their captured variables while they are \
             executing...",
        );
        diag.note("...therefore, they cannot allow references to captured variables to escape");

        diag
    }

    /// Reports a error specifically for when data is escaping a closure.
    ///
    /// ```text
    /// error: borrowed data escapes outside of function
    ///   --> $DIR/lifetime-bound-will-change-warning.rs:44:5
    ///    |
    /// LL | fn test2<'a>(x: &'a Box<Fn()+'a>) {
    ///    |              - `x` is a reference that is only valid in the function body
    /// LL |     // but ref_obj will not, so warn.
    /// LL |     ref_obj(x)
    ///    |     ^^^^^^^^^^ `x` escapes the function body here
    /// ```
    fn report_escaping_data_error(
        &self,
        mbcx: &MirBorrowckCtxt<'_, 'tcx>,
        errci: &ErrorConstraintInfo,
        renctx: &mut RegionErrorNamingCtx,
    ) -> DiagnosticBuilder<'_> {
        let ErrorConstraintInfo { span, category, .. } = errci;

        let fr_name_and_span = self.get_var_name_and_span_for_region(
            mbcx.infcx.tcx,
            &mbcx.body,
            &mbcx.local_names,
            &mbcx.upvars,
            errci.fr,
        );
        let outlived_fr_name_and_span = self.get_var_name_and_span_for_region(
            mbcx.infcx.tcx,
            &mbcx.body,
            &mbcx.local_names,
            &mbcx.upvars,
            errci.outlived_fr,
        );

        let escapes_from = match self.universal_regions.defining_ty {
            DefiningTy::Closure(..) => "closure",
            DefiningTy::Generator(..) => "generator",
            DefiningTy::FnDef(..) => "function",
            DefiningTy::Const(..) => "const",
        };

        // Revert to the normal error in these cases.
        // Assignments aren't "escapes" in function items.
        if (fr_name_and_span.is_none() && outlived_fr_name_and_span.is_none())
            || (*category == ConstraintCategory::Assignment && escapes_from == "function")
            || escapes_from == "const"
        {
            return self.report_general_error(
                mbcx,
                &ErrorConstraintInfo { fr_is_local: true, outlived_fr_is_local: false, ..*errci },
                renctx,
            );
        }

        let mut diag =
            borrowck_errors::borrowed_data_escapes_closure(mbcx.infcx.tcx, *span, escapes_from);

        if let Some((Some(outlived_fr_name), outlived_fr_span)) = outlived_fr_name_and_span {
            diag.span_label(
                outlived_fr_span,
                format!(
                    "`{}` is declared here, outside of the {} body",
                    outlived_fr_name, escapes_from
                ),
            );
        }

        if let Some((Some(fr_name), fr_span)) = fr_name_and_span {
            diag.span_label(
                fr_span,
                format!(
                    "`{}` is a reference that is only valid in the {} body",
                    fr_name, escapes_from
                ),
            );

            diag.span_label(*span, format!("`{}` escapes the {} body here", fr_name, escapes_from));
        }

        diag
    }

    /// Reports a region inference error for the general case with named/synthesized lifetimes to
    /// explain what is happening.
    ///
    /// ```text
    /// error: unsatisfied lifetime constraints
    ///   --> $DIR/regions-creating-enums3.rs:17:5
    ///    |
    /// LL | fn mk_add_bad1<'a,'b>(x: &'a ast<'a>, y: &'b ast<'b>) -> ast<'a> {
    ///    |                -- -- lifetime `'b` defined here
    ///    |                |
    ///    |                lifetime `'a` defined here
    /// LL |     ast::add(x, y)
    ///    |     ^^^^^^^^^^^^^^ function was supposed to return data with lifetime `'a` but it
    ///    |                    is returning data with lifetime `'b`
    /// ```
    fn report_general_error(
        &self,
        mbcx: &MirBorrowckCtxt<'_, 'tcx>,
        errci: &ErrorConstraintInfo,
        renctx: &mut RegionErrorNamingCtx,
    ) -> DiagnosticBuilder<'_> {
        let ErrorConstraintInfo {
            fr,
            fr_is_local,
            outlived_fr,
            outlived_fr_is_local,
            span,
            category,
            ..
        } = errci;

        let mut diag =
            mbcx.infcx.tcx.sess.struct_span_err(*span, "lifetime may not live long enough");

        let mir_def_name =
            if mbcx.infcx.tcx.is_closure(mbcx.mir_def_id) { "closure" } else { "function" };

        let fr_name = self.give_region_a_name(mbcx, renctx, *fr).unwrap();
        fr_name.highlight_region_name(&mut diag);
        let outlived_fr_name = self.give_region_a_name(mbcx, renctx, *outlived_fr).unwrap();
        outlived_fr_name.highlight_region_name(&mut diag);

        match (category, outlived_fr_is_local, fr_is_local) {
            (ConstraintCategory::Return, true, _) => {
                diag.span_label(
                    *span,
                    format!(
                        "{} was supposed to return data with lifetime `{}` but it is returning \
                         data with lifetime `{}`",
                        mir_def_name, outlived_fr_name, fr_name
                    ),
                );
            }
            _ => {
                diag.span_label(
                    *span,
                    format!(
                        "{}requires that `{}` must outlive `{}`",
                        category.description(),
                        fr_name,
                        outlived_fr_name,
                    ),
                );
            }
        }

        self.add_static_impl_trait_suggestion(mbcx.infcx, &mut diag, *fr, fr_name, *outlived_fr);

        diag
    }

    /// Adds a suggestion to errors where a `impl Trait` is returned.
    ///
    /// ```text
    /// help: to allow this `impl Trait` to capture borrowed data with lifetime `'1`, add `'_` as
    ///       a constraint
    ///    |
    /// LL |     fn iter_values_anon(&self) -> impl Iterator<Item=u32> + 'a {
    ///    |                                   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    /// ```
    fn add_static_impl_trait_suggestion(
        &self,
        infcx: &InferCtxt<'_, 'tcx>,
        diag: &mut DiagnosticBuilder<'_>,
        fr: RegionVid,
        // We need to pass `fr_name` - computing it again will label it twice.
        fr_name: RegionName,
        outlived_fr: RegionVid,
    ) {
        if let (Some(f), Some(ty::RegionKind::ReStatic)) =
            (self.to_error_region(fr), self.to_error_region(outlived_fr))
        {
            if let Some((ty::TyS { kind: ty::Opaque(did, substs), .. }, _)) = infcx
                .tcx
                .is_suitable_region(f)
                .map(|r| r.def_id)
                .map(|id| infcx.tcx.return_type_impl_trait(id))
                .unwrap_or(None)
            {
                // Check whether or not the impl trait return type is intended to capture
                // data with the static lifetime.
                //
                // eg. check for `impl Trait + 'static` instead of `impl Trait`.
                let has_static_predicate = {
                    let predicates_of = infcx.tcx.predicates_of(*did);
                    let bounds = predicates_of.instantiate(infcx.tcx, substs);

                    let mut found = false;
                    for predicate in bounds.predicates {
                        if let ty::Predicate::TypeOutlives(binder) = predicate {
                            if let ty::OutlivesPredicate(_, ty::RegionKind::ReStatic) =
                                binder.skip_binder()
                            {
                                found = true;
                                break;
                            }
                        }
                    }

                    found
                };

                debug!(
                    "add_static_impl_trait_suggestion: has_static_predicate={:?}",
                    has_static_predicate
                );
                let static_str = kw::StaticLifetime;
                // If there is a static predicate, then the only sensible suggestion is to replace
                // fr with `'static`.
                if has_static_predicate {
                    diag.help(&format!("consider replacing `{}` with `{}`", fr_name, static_str));
                } else {
                    // Otherwise, we should suggest adding a constraint on the return type.
                    let span = infcx.tcx.def_span(*did);
                    if let Ok(snippet) = infcx.tcx.sess.source_map().span_to_snippet(span) {
                        let suggestable_fr_name = if fr_name.was_named() {
                            fr_name.to_string()
                        } else {
                            "'_".to_string()
                        };
                        let suggestion = if snippet.ends_with(";") {
                            // `type X = impl Trait;`
                            format!("{} + {};", &snippet[..snippet.len() - 1], suggestable_fr_name)
                        } else {
                            format!("{} + {}", snippet, suggestable_fr_name)
                        };
                        diag.span_suggestion(
                            span,
                            &format!(
                                "to allow this `impl Trait` to capture borrowed data with lifetime \
                                 `{}`, add `{}` as a bound",
                                fr_name, suggestable_fr_name,
                            ),
                            suggestion,
                            Applicability::MachineApplicable,
                        );
                    }
                }
            }
        }
    }
}
