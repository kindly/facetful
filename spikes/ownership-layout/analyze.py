"""Read-only structural profile of a large ownership-graph workbook
(August 2026 public tracker download; file kept locally, not in the repo).

Uses XLSX XML directly; no database engine or workbook mutation. Report byte
estimates describe proposed arrays, not measured compiled-file sizes.
"""
import collections
import hashlib
import json
import math
from pathlib import Path
import sys
import xml.etree.ElementTree as ET
from zipfile import ZipFile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / 'data/ownership/ownership-2026-08.xlsx'
OUTPUT = ROOT / 'data/ownership/layout-profile.json'
NS = {'s': 'http://schemas.openxmlformats.org/spreadsheetml/2006/main'}


def degree_stats(values):
    v = sorted(values)
    return {'count': len(v), 'zero': v.count(0), 'one': v.count(1),
            'two': v.count(2), 'at_most_two': sum(x <= 2 for x in v),
            'mean': sum(v) / len(v) if v else 0,
            **{f'p{q}': v[min(len(v)-1, math.ceil(q*len(v)/100)-1)] if v else 0
               for q in [50, 90, 95, 99, 100]}}


def main():
    with ZipFile(SOURCE) as z:
        strings = [''.join(t.text or '' for t in e.findall('.//s:t', NS))
                   for e in ET.fromstring(z.read('xl/sharedStrings.xml')).findall('s:si', NS)]
        rels = {r.attrib['Id']: r.attrib['Target']
                for r in ET.fromstring(z.read('xl/_rels/workbook.xml.rels'))}
        sheets = {}
        for s in ET.fromstring(z.read('xl/workbook.xml')).findall('.//s:sheet', NS):
            target = rels[s.attrib['{http://schemas.openxmlformats.org/officeDocument/2006/relationships}id']]
            sheets[s.attrib['name']] = target.lstrip('/') if target.startswith('/') else 'xl/' + target

        def rows(name):
            headers = None
            with z.open(sheets[name]) as f:
                for _, row in ET.iterparse(f, events=['end']):
                    if row.tag != '{' + NS['s'] + '}row':
                        continue
                    values = {}
                    for c in row.findall('s:c', NS):
                        v = c.find('s:v', NS)
                        value = (v.text or '') if v is not None else ''
                        if c.attrib.get('t') == 's':
                            value = strings[int(value)]
                        elif c.attrib.get('t') == 'inlineStr':
                            value = ''.join(t.text or '' for t in c.findall('.//s:t', NS))
                        if value:
                            values[''.join(x for x in c.attrib['r'] if x.isalpha())] = value
                    if headers is None:
                        headers = values
                    elif values.get('A'):
                        yield {headers.get(k, k): v for k, v in values.items()}
                    row.clear()

        entities = list(rows('All Entities'))
        entity_edges = list(rows('Entity Ownership'))
        asset_edges = list(rows('Asset Ownership'))
        print('Loaded the three normalized graph sheets.', flush=True)
        names = {r['Entity ID']: r.get('Full Name', '') for r in entities}
        ids = list(names)
        forward = {i: set() for i in ids}
        reverse = {i: set() for i in ids}
        entity_pairs = collections.Counter()
        self_links = []
        for r in entity_edges:
            parent, child = r['Interested Party ID'], r['Subject Entity ID']
            assert parent in names and child in names
            forward[parent].add(child)
            reverse[child].add(parent)
            entity_pairs[parent, child] += 1
            if parent == child:
                self_links.append(parent)

        # Iterative Kosaraju: do not assume the ownership graph is a tree/DAG.
        visited, finish = set(), []
        for start in ids:
            if start in visited:
                continue
            visited.add(start)
            stack = [(start, iter(forward[start]))]
            while stack:
                node, children = stack[-1]
                nxt = next(children, None)
                if nxt is None:
                    finish.append(node)
                    stack.pop()
                elif nxt not in visited:
                    visited.add(nxt)
                    stack.append((nxt, iter(forward[nxt])))
        comp_of, components = {}, []
        for start in reversed(finish):
            if start in comp_of:
                continue
            index = len(components)
            members, stack = [], [start]
            comp_of[start] = index
            while stack:
                node = stack.pop()
                members.append(node)
                for nxt in reverse[node]:
                    if nxt not in comp_of:
                        comp_of[nxt] = index
                        stack.append(nxt)
            components.append(members)
        cyclic = sorted([c for c in components if len(c) > 1], key=len, reverse=True)

        assets, asset_parents, owned_assets = {}, collections.defaultdict(set), collections.defaultdict(set)
        asset_pairs = collections.Counter()
        for r in asset_edges:
            key = (r.get('Asset Type', ''), r.get('Asset ID', ''), r.get('Asset Unit ID', ''))
            owner = r['Immediate Owner Entity ID']
            assert owner in names
            assets.setdefault(key, r.get('Asset Name', ''))
            asset_parents[key].add(owner)
            owned_assets[owner].add(key)
            asset_pairs[owner, key] += 1

        # Weak components are relevant to whether independent portfolios can be clustered.
        weak, seen = [], set()
        for start in ids:
            if start in seen:
                continue
            seen.add(start)
            stack, members, leaves = [start], [], set()
            while stack:
                node = stack.pop()
                members.append(node)
                for nxt in forward[node] | reverse[node]:
                    if nxt not in seen:
                        seen.add(nxt)
                        stack.append(nxt)
                for asset in owned_assets[node]:
                    leaves.add(asset)
                    for nxt in asset_parents[asset]:
                        if nxt not in seen:
                            seen.add(nxt)
                            stack.append(nxt)
            weak.append({'entities': len(members), 'asset_units': len(leaves)})
        weak.sort(key=lambda c: c['entities'] + c['asset_units'], reverse=True)

        string_sizes = {}
        for field in sorted({k for r in entities for k in r}):
            vals = [r.get(field, '') for r in entities]
            nonempty = [v for v in vals if v]
            string_sizes[field] = {'nonempty': len(nonempty), 'distinct': len(set(nonempty)),
                                   'utf8_bytes': sum(len(v.encode()) for v in vals),
                                   'distinct_utf8_bytes': sum(len(v.encode()) for v in set(vals))}
        flags = {}
        for label, table in [('entity', entity_edges), ('asset', asset_edges)]:
            flags[label] = {'imputation': dict(collections.Counter(r.get('Share Imputed?', '') for r in table)),
                            'missing_share': sum(not r.get('% Share of Ownership', '') for r in table)}
            shares = [float(r['% Share of Ownership']) for r in table if r.get('% Share of Ownership', '')]
            flags[label]['share_min_max'] = [min(shares), max(shares)]
            flags[label]['shares_not_exact_hundredths'] = sum(abs(x*100-round(x*100)) > 1e-7 for x in shares)

        n, e = len(ids) + len(assets), len(entity_edges) + len(asset_edges)
        report = {
            'source': SOURCE.name, 'sha256': hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
            'nodes': {'entities': len(ids), 'asset_type_id_unit_tuples': len(assets), 'total': n},
            'edges': {'entity_rows': len(entity_edges), 'entity_unique_endpoint_pairs': len(entity_pairs),
                      'entity_repeated_endpoint_rows': sum(v-1 for v in entity_pairs.values()),
                      'asset_rows': len(asset_edges), 'asset_unique_endpoint_pairs': len(asset_pairs),
                      'asset_repeated_endpoint_rows': sum(v-1 for v in asset_pairs.values())},
            'degree': {'entity_owners': degree_stats([len(reverse[i]) for i in ids]),
                       'entity_owned_entities': degree_stats([len(forward[i]) for i in ids]),
                       'entity_direct_asset_units': degree_stats([len(owned_assets[i]) for i in ids]),
                       'asset_unit_owners': degree_stats([len(asset_parents[a]) for a in assets])},
            'cycles': {'self_links': self_links, 'nontrivial_scc_count': len(cyclic),
                       'entities_in_nontrivial_sccs': sum(map(len, cyclic)),
                       'largest_scc_size': max(map(len, cyclic), default=0),
                       'examples': [[{'id': i, 'name': names[i]} for i in c[:12]] for c in cyclic[:3]]},
            'weak_components': {'count': len(weak), 'largest': weak[:5]},
            'top_direct_owners': [{'id': i, 'name': names[i], 'owned_entities': len(forward[i]),
                                   'direct_asset_units': len(owned_assets[i])}
                                  for i in sorted(ids, key=lambda i: len(forward[i])+len(owned_assets[i]), reverse=True)[:10]],
            'entity_string_fields': string_sizes, 'shares': flags,
            'edge_source_url_utf8_bytes': sum(len(r.get('Data Source URL', '').encode()) for r in entity_edges),
            'asset_name_utf8_bytes_once_per_key': sum(len(v.encode()) for v in assets.values()),
            'proposed_core_byte_estimate': {
                'notes': 'Uncompressed packed arrays, retaining every source edge row. Excludes properties, external IDs, file metadata, decoder workspace and query state.',
                'two_csr_offset_arrays_u32': 8*(n+1),
                'two_neighbour_and_edgeid_arrays_u32': 16*e,
                'canonical_share_f64_and_flags_u8': 9*e,
                'total': 8*(n+1)+25*e,
                'optional_source_locator_u32': 4*e},
        }

        # Profile expanded export size; do not sum capacities across repeated paths.
        expanded = {}
        for name in sheets:
            if name in ['About', 'All Entities', 'Entity Ownership', 'Asset Ownership']:
                continue
            count = path_bytes = 0
            measures = set()
            for r in rows(name):
                count += 1
                path_bytes += len(r.get('Ownership Path', '').encode())
                measures.update(k for k in r if any(s in k.lower() for s in ['capacity', 'production', 'status']))
            expanded[name] = {'rows': count, 'ownership_path_utf8_bytes': path_bytes,
                              'measure_and_status_columns': sorted(measures),
                              'xlsx_member_compressed_bytes': z.getinfo(sheets[name]).compress_size}
            print(f'Profiled {name}: {count:,} expanded rows.', flush=True)
        report['expanded_sheets'] = expanded
        OUTPUT.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
        print(json.dumps({k: v for k, v in report.items() if k not in ['entity_string_fields', 'expanded_sheets']},
                         ensure_ascii=False, indent=2))
        print('Report:', OUTPUT)


if __name__ == '__main__':
    main()
