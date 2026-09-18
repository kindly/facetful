# map_plant_fuel
select plant_id_eia as p, fuel_type_code_pudl as k, min(latitude) as lat, min(longitude) as lon, round(sum(net_generation_mwh)/1000000.0, 4) as v from t where latitude is not null group by plant_id_eia, fuel_type_code_pudl order by v desc

# map_plant_only
select plant_id_eia as p, count(*) as n, round(sum(net_generation_mwh)/1000.0, 1) as v from t where latitude is not null group by plant_id_eia order by v desc

# plant_fuel_state
select plant_id_eia as p, fuel_type_code_pudl as k, state as s, count(*) as n from t group by plant_id_eia, fuel_type_code_pudl, state

# fuel_state_2dim
select fuel_type_code_pudl as k, state as s, count(*) as n, round(sum(net_generation_mwh)/1000.0, 1) as v from t group by fuel_type_code_pudl, state order by v desc

# plantname_fuel_text
select plant_name_eia as p, fuel_type_code_pudl as k, count(*) as n from t group by plant_name_eia, fuel_type_code_pudl

# pf_count_fused
select plant_id_eia as p, fuel_type_code_pudl as k, count(*) as n from t group by plant_id_eia, fuel_type_code_pudl

# pf_sum
select plant_id_eia as p, fuel_type_code_pudl as k, sum(net_generation_mwh) as v from t group by plant_id_eia, fuel_type_code_pudl

# pf_sum_order_int
select plant_id_eia as p, fuel_type_code_pudl as k, count(*) as n, sum(net_generation_mwh) as v from t group by plant_id_eia, fuel_type_code_pudl order by n desc

# pf_sum_order_float_expr
select plant_id_eia as p, fuel_type_code_pudl as k, count(*) as n, round(sum(net_generation_mwh)/1000.0, 1) as v from t group by plant_id_eia, fuel_type_code_pudl order by v desc

# pf_sum_order_limit
select plant_id_eia as p, fuel_type_code_pudl as k, count(*) as n, round(sum(net_generation_mwh)/1000.0, 1) as v from t group by plant_id_eia, fuel_type_code_pudl order by v desc limit 100
