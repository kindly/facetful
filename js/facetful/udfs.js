// Ready-made user-defined functions (design.sv d49/d51): things the browser
// does natively that would cost the wasm engine tens or hundreds of KB —
// JSON, the Intl tables (timezones, country names, number formats), Unicode
// normalization, URL parsing — plus the temporal long tail over Date.UTC.
//
// Registered by default by Facetful.open() and the `facetful` command
// (`open({ udfs: false })` opts out). The list is exported for callers that
// drive core.js directly or want a subset.
//
// Each entry is { name, signature, fn }. Functions are self-contained (they run
// in the worker from their source), vectorized where it pays, per-row where it
// reads better. Dates are days since 1970-01-01, timestamps ms since the epoch.

const DAY = 86400000;

export const udfs = [
  // --- regular expressions ---------------------------------------------------
  // regexp(s, pattern[, flags]) -> bool, the browser's RegExp (JIT-compiled, cached per
  // pattern); the native CLI answers the same SQL through the `regex` crate
  {
    name: "regexp",
    signature: { params: ["text", "text"], returns: "bool", variadic: true },
    fn: (() => {
      const cache = new Map();
      return (args, len, out) => {
        const [s, p] = args;
        const key = p.values[0] + "\u0000" + (args[2] ? args[2].values[0] : "");
        let re = cache.get(key);
        if (!re) cache.set(key, (re = new RegExp(p.values[0], args[2] ? args[2].values[0] : "")));
        for (let i = 0; i < len; i++) out.values[i] = re.test(s.values[i]) ? 1 : 0;
      };
    })(),
  },
  // --- JSON ---------------------------------------------------------------
  // json_extract(doc, '$.a.b[0]') -> text (numbers/bools stringified, null for missing)
  {
    name: "json_extract",
    signature: { params: ["text", "text"], returns: "text" },
    fn: (() => {
      const paths = new Map();
      const parsePath = (p) => {
        let keys = paths.get(p);
        if (!keys) {
          keys = [];
          for (const m of p.replace(/^\$\.?/, "").matchAll(/([^.[\]]+)|\[(\d+)\]/g)) keys.push(m[2] !== undefined ? Number(m[2]) : m[1]);
          paths.set(p, keys);
        }
        return keys;
      };
      return (args, len, out) => {
        const [doc, path] = args;
        const keys = parsePath(path.values[0]);
        for (let i = 0; i < len; i++) {
          let v;
          try { v = JSON.parse(doc.values[i]); } catch { out.values[i] = null; continue; }
          for (const k of keys) { if (v == null) break; v = v[k]; }
          out.values[i] = v == null ? null : typeof v === "object" ? JSON.stringify(v) : String(v);
        }
      };
    })(),
  },
  // --- time zones and the temporal long tail -------------------------------
  // to_tz(ts, 'America/New_York') -> text "YYYY-MM-DD HH:MM:SS" in that zone (Intl's IANA tables)
  {
    name: "to_tz",
    signature: { params: ["timestamp", "text"], returns: "text" },
    fn: (() => {
      const fmts = new Map();
      return (args, len, out) => {
        const [ts, tz] = args;
        const zone = tz.values[0];
        let f = fmts.get(zone);
        if (!f) fmts.set(zone, (f = new Intl.DateTimeFormat("sv-SE", { timeZone: zone, year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false })));
        for (let i = 0; i < len; i++) out.values[i] = f.format(ts.values[i]);
      };
    })(),
  },
  // date_trunc('month', ts) -> timestamp at the start of the unit (UTC); units: year quarter month week day hour
  {
    name: "date_trunc",
    signature: { params: ["text", "timestamp"], returns: "timestamp" },
    fn: (args, len, out) => {
      const [unit, ts] = args;
      const u = unit.values[0];
      for (let i = 0; i < len; i++) {
        const d = new Date(ts.values[i]);
        let r;
        switch (u) {
          case "year": r = Date.UTC(d.getUTCFullYear(), 0, 1); break;
          case "quarter": r = Date.UTC(d.getUTCFullYear(), d.getUTCMonth() - (d.getUTCMonth() % 3), 1); break;
          case "month": r = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), 1); break;
          case "week": { const dow = (d.getUTCDay() + 6) % 7; r = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate() - dow); break; } // Monday
          case "day": r = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()); break;
          case "hour": r = ts.values[i] - (ts.values[i] % 3600000); break;
          default: throw new Error(`date_trunc: unknown unit '${u}'`);
        }
        out.values[i] = r;
      }
    },
  },
  // date_add(d, n, 'month') -> date; units: day week month year (month arithmetic clamps to the month's end)
  {
    name: "date_add",
    signature: { params: ["date", "int", "text"], returns: "date", perRow: true },
    fn: (d, n, unit) => {
      const t = new Date(d * DAY);
      switch (unit) {
        case "day": return d + n;
        case "week": return d + 7 * n;
        case "month": case "year": {
          const months = unit === "year" ? 12 * n : n;
          const y = t.getUTCFullYear(), m = t.getUTCMonth() + months, day = t.getUTCDate();
          const last = new Date(Date.UTC(y, m + 1, 0)).getUTCDate();
          return Date.UTC(y, m, Math.min(day, last)) / DAY;
        }
        default: throw new Error(`date_add: unknown unit '${unit}'`);
      }
    },
  },
  // weekday(d) -> int, 0 = Monday … 6 = Sunday; quarter(d) -> 1..4
  { name: "weekday", signature: { params: ["date"], returns: "int" }, fn: (args, len, out) => { for (let i = 0; i < len; i++) out.values[i] = (new Date(args[0].values[i] * DAY).getUTCDay() + 6) % 7; } },
  { name: "quarter", signature: { params: ["date"], returns: "int" }, fn: (args, len, out) => { for (let i = 0; i < len; i++) out.values[i] = Math.floor(new Date(args[0].values[i] * DAY).getUTCMonth() / 3) + 1; } },
  // --- Intl tables -----------------------------------------------------------
  // country_name('DE') -> 'Germany' (ISO 3166 alpha-2; optional locale second arg)
  {
    name: "country_name",
    signature: { params: ["text"], returns: "text", variadic: true },
    fn: (() => {
      const names = new Map();
      return (args, len, out) => {
        const locale = args.length > 1 ? args[1].values[0] : "en";
        let dn = names.get(locale);
        if (!dn) names.set(locale, (dn = new Intl.DisplayNames([locale], { type: "region" })));
        for (let i = 0; i < len; i++) {
          const code = args[0].values[i].toUpperCase();
          try { out.values[i] = /^[A-Z]{2}$/.test(code) ? dn.of(code) : null; } catch { out.values[i] = null; }
        }
      };
    })(),
  },
  // format_number(x, 'en-US') -> '1,234,567.9' ; compact form via 'en-US:compact' -> '1.2M'
  {
    name: "format_number",
    signature: { params: ["float", "text"], returns: "text" },
    fn: (() => {
      const fmts = new Map();
      return (args, len, out) => {
        const spec = args[1].values[0];
        let f = fmts.get(spec);
        if (!f) {
          const [locale, style] = spec.split(":");
          fmts.set(spec, (f = new Intl.NumberFormat(locale, style === "compact" ? { notation: "compact", maximumFractionDigits: 1 } : { maximumFractionDigits: 1 })));
        }
        for (let i = 0; i < len; i++) out.values[i] = f.format(args[0].values[i]);
      };
    })(),
  },
  // --- Unicode and URLs -------------------------------------------------------
  // unaccent('Zürich') -> 'Zurich' : NFD then strip combining marks (\p{M} — the regex class wasm can't afford)
  { name: "unaccent", signature: { params: ["text"], returns: "text" }, fn: (args, len, out) => { for (let i = 0; i < len; i++) out.values[i] = args[0].values[i].normalize("NFD").replace(/\p{M}+/gu, ""); } },
  // url_host('https://www.eia.gov/x?y') -> 'www.eia.gov' (null when not a URL)
  { name: "url_host", signature: { params: ["text"], returns: "text", perRow: true }, fn: (s) => { try { return new URL(s).hostname; } catch { return null; } } },
];

export default udfs;
