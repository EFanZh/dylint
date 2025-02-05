use super::{IGNORED_INHERENTS, WATCHED_INHERENTS};
use clippy_utils::{def_path_res, get_trait_def_id, match_def_path};
use if_chain::if_chain;
use rustc_hir::{def_id::DefId, Unsafety};
use rustc_lint::LateContext;
use rustc_middle::ty::{
    self,
    fold::{BottomUpFolder, TypeFolder},
};
use rustc_span::symbol::sym;

pub fn check_inherents<I: Iterator<Item = DefId>>(cx: &LateContext<'_>, inherent_def_ids: I) {
    let into_iterator_def_id =
        get_trait_def_id(cx, &["core", "iter", "traits", "collect", "IntoIterator"]).unwrap();
    let iterator_def_id =
        get_trait_def_id(cx, &["core", "iter", "traits", "iterator", "Iterator"]).unwrap();

    let mut type_paths = WATCHED_INHERENTS
        .iter()
        .filter_map(|path| {
            if is_primitive_impl(path) || path.first() == Some(&"tempfile") {
                return None;
            }
            Some(path.split_last().unwrap().1)
        })
        .collect::<Vec<_>>();

    type_paths.dedup();

    let of_interest = |def_id| -> bool {
        if cx.tcx.visibility(def_id) != ty::Visibility::Public {
            return false;
        }

        let assoc_item = cx.tcx.associated_item(def_id);
        if assoc_item.kind != ty::AssocKind::Fn {
            return false;
        }

        let fn_sig = cx.tcx.fn_sig(assoc_item.def_id).skip_binder();
        if fn_sig.unsafety() == Unsafety::Unsafe || fn_sig.skip_binder().inputs().len() != 1 {
            return false;
        }

        let input_ty = cx.tcx.erase_late_bound_regions(fn_sig.input(0));
        let output_ty = cx.tcx.erase_late_bound_regions(fn_sig.output());

        if let Some(input_item_ty) = implements_trait_with_item(cx, input_ty, into_iterator_def_id)
        {
            if let Some(output_item_ty) = implements_trait_with_item(cx, output_ty, iterator_def_id)
                && input_item_ty == output_item_ty
            {
                return true;
            }
        } else {
            // smoelius: Sanity.
            assert!(!input_ty.to_string().starts_with("std::vec::Vec"));
        }

        [input_ty, output_ty].into_iter().all(|ty| {
            let ty = peel_unwanted(cx, def_id, ty);
            ty.is_slice()
                || ty.is_str()
                || ty.ty_adt_def().map_or(false, |adt_def| {
                    type_paths
                        .iter()
                        .any(|path| match_def_path(cx, adt_def.did(), path))
                })
        })
    };

    // smoelius: Watched and ignored inherents are "of interest."
    for path in WATCHED_INHERENTS.iter().chain(IGNORED_INHERENTS.iter()) {
        if is_primitive_impl(path) || path.first() == Some(&"tempfile") {
            continue;
        }

        let def_id = def_path_res(cx, path)
            .into_iter()
            .find_map(|res| res.opt_def_id())
            .unwrap();

        assert!(
            of_interest(def_id),
            "{:?} is not of interest",
            cx.get_def_path(def_id)
        );
    }

    // smoelius: Watched inherents are complete(ish).
    for impl_def_id in type_paths
        .iter()
        .flat_map(|type_path| def_path_res(cx, type_path))
        .filter_map(|res| res.opt_def_id())
        .flat_map(|def_id| cx.tcx.inherent_impls(def_id))
        .copied()
        .chain(inherent_def_ids.map(|def_id| cx.tcx.parent(def_id)))
    {
        for &assoc_item_def_id in cx.tcx.associated_item_def_ids(impl_def_id) {
            if of_interest(assoc_item_def_id) {
                assert!(
                    WATCHED_INHERENTS
                        .iter()
                        .chain(IGNORED_INHERENTS.iter())
                        .any(|path| match_def_path(cx, assoc_item_def_id, path)),
                    "{:?} is missing",
                    cx.get_def_path(assoc_item_def_id)
                );
            }
        }
    }
}

fn is_primitive_impl(path: &[&str]) -> bool {
    path.iter().any(|s| s.starts_with('<'))
}

fn implements_trait_with_item<'tcx>(
    cx: &LateContext<'tcx>,
    ty: ty::Ty<'tcx>,
    trait_id: DefId,
) -> Option<ty::Ty<'tcx>> {
    cx.get_associated_type(replace_params_with_global_ty(cx, ty), trait_id, "Item")
}

// smoelius: This is a hack. For `get_associated_type` to return `Some(..)`, all of its argument
// type's type parameters must be substituted for. One of the types of interest is `Vec`, and its
// second type parameter must implement `alloc::alloc::Allocator`. So we instantiate all type
// parameters with the default `Allocator`, `alloc::alloc::Global`. A more robust solution would
// at least consider trait bounds and alert when a trait other than `Allocator` was encountered.
fn replace_params_with_global_ty<'tcx>(cx: &LateContext<'tcx>, ty: ty::Ty<'tcx>) -> ty::Ty<'tcx> {
    let global_def_id = def_path_res(cx, &["alloc", "alloc", "Global"])
        .into_iter()
        .find_map(|res| res.opt_def_id())
        .unwrap();
    let global_adt_def = cx.tcx.adt_def(global_def_id);
    let global_ty = cx.tcx.mk_adt(global_adt_def, ty::List::empty());
    BottomUpFolder {
        tcx: cx.tcx,
        ty_op: |ty| {
            if matches!(ty.kind(), ty::Param(_)) {
                global_ty
            } else {
                ty
            }
        },
        lt_op: std::convert::identity,
        ct_op: std::convert::identity,
    }
    .fold_ty(ty)
}

fn peel_unwanted<'tcx>(
    cx: &LateContext<'tcx>,
    def_id: DefId,
    mut ty: ty::Ty<'tcx>,
) -> ty::Ty<'tcx> {
    const BOX: [&str; 3] = ["alloc", "boxed", "Box"];

    loop {
        match ty.kind() {
            ty::Ref(_, referent_ty, _) => {
                ty = *referent_ty;
                continue;
            }
            ty::Adt(adt_def, substs) if match_def_path(cx, adt_def.did(), &BOX) => {
                ty = substs[0].expect_ty();
                continue;
            }
            _ => {}
        }

        if let Some(as_ref_ty) = strip_as_ref(cx, def_id, ty) {
            ty = as_ref_ty;
            continue;
        }

        break;
    }

    ty
}

fn strip_as_ref<'tcx>(
    cx: &LateContext<'tcx>,
    def_id: DefId,
    ty: ty::Ty<'tcx>,
) -> Option<ty::Ty<'tcx>> {
    cx.tcx
        .param_env(def_id)
        .caller_bounds()
        .iter()
        .find_map(|predicate| {
            if_chain! {
                if let ty::ClauseKind::Trait(ty::TraitPredicate {
                    trait_ref,
                    ..
                }) = predicate.kind().skip_binder();
                if cx.tcx.get_diagnostic_item(sym::AsRef) == Some(trait_ref.def_id);
                if let [self_arg, subst_arg] = trait_ref.substs.as_slice();
                if self_arg.unpack() == ty::GenericArgKind::Type(ty);
                if let ty::GenericArgKind::Type(subst_ty) = subst_arg.unpack();
                then {
                    Some(subst_ty)
                } else {
                    None
                }
            }
        })
}
