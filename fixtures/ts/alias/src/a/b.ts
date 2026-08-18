import { util } from "../util";
import { x } from "~/lib/x";
import { useState } from "react";

export function b(): number {
  return util() + (x() ? 1 : 0);
}
