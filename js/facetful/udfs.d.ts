import type { UdfSignature, UdfVectorFn, UdfRowFn } from "./index.js";

export interface ReadyMadeUdf {
  name: string;
  signature: UdfSignature;
  fn: UdfVectorFn | UdfRowFn;
}

/**
 * The ready-made functions Facetful.open() registers by default: regexp,
 * json_extract, to_tz, date_trunc, date_add, weekday, quarter, country_name,
 * format_number, unaccent, url_host.
 */
export const udfs: ReadyMadeUdf[];
export default udfs;
