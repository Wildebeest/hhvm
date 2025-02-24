(*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the "hack" directory of this source tree.
 *
 *)

open Hh_prelude
module Util = Symbol_json_util

type t = {
  kind: SymbolDefinition.kind;
  name: string;
  full_name: string;
}

let resolve ctx occ =
  Option.map
    (ServerSymbolDefinition.go ctx None occ)
    ~f:(fun SymbolDefinition.{ name; full_name; kind; _ } ->
      { kind; name; full_name })

let get_class_by_name ctx class_ =
  match ServerSymbolDefinition.get_class_by_name ctx class_ with
  | None -> `None
  | Some cls when Util.is_enum_or_enum_class cls.Aast.c_kind -> `Enum
  | Some cls -> `Class cls

let get_kind ctx class_ =
  match ServerSymbolDefinition.get_class_by_name ctx class_ with
  | None -> None
  | Some cls -> Some cls.Aast.c_kind
