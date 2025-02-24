(*
 * Copyright (c) 2017, Facebook, Inc.
 * All rights reserved.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)

(* Details about functions to be added in json output *)
type func_param_result = {
  param_name: string;
  param_ty: string;
  param_variadic: bool;
}
[@@deriving show]

type func_details_result = {
  params: func_param_result list;
  return_ty: string;
  min_arity: int;
}
[@@deriving show]

type ranking_details_result = {
  detail: string;
  sort_text: string;
  kind: string;
}

(* Results ready to be displayed to the user *)
type complete_autocomplete_result = {
  (* The position of the declaration we're returning. *)
  res_pos: Pos.absolute;
  (* The position in the opened file that we're replacing with res_name. *)
  res_replace_pos: Ide_api_types.range;
  (* If we're autocompleting a method, store the class name of the variable
        we're calling the method on (for doc block fallback in autocomplete
        resolution). *)
  res_base_class: string option;
  res_ty: string;
  res_name: string;
  (* Without trimming for namespaces *)
  res_fullname: string;
  res_kind: SearchUtils.si_kind;
  func_details: func_details_result option;
  ranking_details: ranking_details_result option;
  (* documentation (in markdown); if absent, then it will be resolved on-demand later *)
  res_documentation: string option;
}

(* The type returned to the client *)
type ide_result = {
  completions: complete_autocomplete_result list;
  char_at_pos: char;
  is_complete: bool;
}

type result = complete_autocomplete_result list

type legacy_autocomplete_context = {
  is_manually_invoked: bool;
  is_xhp_classname: bool;
  is_after_single_colon: bool;
  is_after_double_right_angle_bracket: bool;
  is_after_open_square_bracket: bool;
  is_after_quote: bool;
  is_before_apostrophe: bool;
  is_open_curly_without_equals: bool;
  char_at_pos: char;
}

(* The standard autocomplete token, which is currently "AUTO332" *)
val autocomplete_token : string

(* The length of the standard autocomplete token *)
val autocomplete_token_length : int
