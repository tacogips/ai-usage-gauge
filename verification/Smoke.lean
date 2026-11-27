-- A standalone proof to check that the Lean toolchain is working.
example (xs : List Nat) : xs.reverse.reverse = xs :=
  List.reverse_reverse xs
