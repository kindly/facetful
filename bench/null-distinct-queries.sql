# count_all
select count(*) from t

# distinct_small_dict
select count(distinct country) from t

# distinct_large_dict
select count(distinct owner) from t

# distinct_coalesce
select count(distinct coalesce(owner, 'unknown')) from t

# coalesce_filter
select count(*) from t where coalesce(owner, 'unknown') = 'owner_7'

# bare_filter
select count(*) from t where owner = 'owner_7'

# distinct_filtered
select count(distinct owner) from t where id % 7 = 0

# distinct_grouped
select country, count(distinct owner) from t group by country
