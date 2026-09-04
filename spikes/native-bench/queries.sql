# facet_count
select status, count(*) from t where country in ('country_1','country_2','country_3') group by status order by status

# filtered_total
select count(*), sum(capacity) from t where status in ('status_0','status_1') and year between 'year_10' and 'year_19'

# group_small
select country, count(*) as n, sum(capacity) as total from t group by country order by n desc, country limit 10

# group_two_dims
select region, fuel, count(*) as n, sum(capacity) as s from t group by region, fuel order by region, fuel

# group_high_card
select owner, count(*) as n from t group by owner order by n desc, owner limit 20

# topk
select id, capacity from t where capacity is not null order by capacity desc limit 50

# arith_scan
select avg(capacity * 2 + 1) from t where id % 7 = 0

# like_scan
select count(*) from t where owner like '%7%'

# case_pivot
select country, sum(case when status = 'status_0' then capacity else 0 end) as s0, sum(case when status = 'status_1' then capacity else 0 end) as s1 from t group by country order by country limit 10
