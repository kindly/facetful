# fuel_count
select fuel_type_code_pudl as k, count(*) as n from t group by k order by n desc limit 500

# fuel_1distinct
select fuel_type_code_pudl as k, count(distinct plant_id_eia) as n from t group by k order by n desc limit 500

# fuel_full
select fuel_type_code_pudl as k, count(distinct plant_id_eia) as n, round(sum(net_generation_mwh)/1000000.0, 3) as v from t group by k order by v desc limit 500

# state_1distinct
select state as k, count(distinct plant_id_eia) as n from t group by k order by n desc limit 500

# state_full
select state as k, count(distinct plant_id_eia) as n, round(sum(net_generation_mwh)/1000000.0, 3) as v from t group by k order by v desc limit 500

# utility_count
select utility_name_eia as k, count(*) as n from t group by k order by n desc limit 300

# utility_1distinct
select utility_name_eia as k, count(distinct plant_id_eia) as n from t group by k order by n desc limit 300

# utility_full
select utility_name_eia as k, count(distinct plant_id_eia) as n, round(sum(net_generation_mwh)/1000000.0, 3) as v from t group by k order by v desc limit 300

# totals_unfiltered
select count(distinct plant_id_eia) as plants from t

# year_fuel_pivot
select year as y, fuel_type_code_pudl as k, count(distinct plant_id_eia) as n from t group by y, k order by y limit 1000

# map_plant_fuel
select plant_id_eia as p, fuel_type_code_pudl as k, count(distinct utility_id_eia) as n from t group by p, k order by p limit 30000
