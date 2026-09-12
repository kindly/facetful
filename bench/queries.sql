# facet_count
select "Status", count(*) as n, sum("Capacity (MW)") as mw from t group by "Status"

# facet_filtered
select "Country/area", count(*) as n from t where "Status" = 'operating' group by "Country/area" order by n desc limit 20

# pivot
select "Status", "Technology", count(*) as n from t group by "Status", "Technology"

# year_hist
select "Start year", count(*) as n from t where "Start year" >= 1990 group by "Start year"

# topk
select "Plant / Project name", "Country/area", "Capacity (MW)" from t order by "Capacity (MW)" desc limit 100

# full_sort
select "Plant / Project name", "Capacity (MW)" from t order by "Capacity (MW)" desc

# like_dict
select count(*) from t where "Owner(s)" like '%green%'

# like_blob
select count(*) from t where "Plant / Project name" like '%green%'

# like_2col_facet
select "Status", count(*) as n from t where "Plant / Project name" like '%green%' or "Owner(s)" like '%green%' group by "Status"

# cast_substr
select count(*) from t where cast(substr("GEM unit/phase ID", 2) as int) % 7 = 0

# projection_floats
select "Latitude", "Longitude", "Capacity (MW)" from t

# projection_text
select "Plant / Project name", "GEM unit/phase ID" from t

# select_star_limit
select * from t limit 100

# in_list_dict
select count(*) from t where "Status" in ('operating', 'construction', 'announced')
