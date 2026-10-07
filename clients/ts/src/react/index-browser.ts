import { dial } from "../dial-browser.js";
import { useFHIRQueryWith } from "./useFHIRQuery.js";
import type { UseFHIRQueryOptions, UseFHIRQueryResult } from "./useFHIRQuery.js";

export type { UseFHIRQueryOptions, UseFHIRQueryResult };

export function useFHIRQuery(options: UseFHIRQueryOptions): UseFHIRQueryResult {
  return useFHIRQueryWith(dial, options);
}
