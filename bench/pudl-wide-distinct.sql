# fuel_count
select fuel_type_code_pudl as k, count(*) as n from t group by k order by n desc limit 500

# fuel_1distinct
select fuel_type_code_pudl as k, count(distinct plant_id_eia) as n from t group by k order by n desc limit 500

# fuel_2distinct
select fuel_type_code_pudl as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g from t group by k order by n desc limit 500

# fuel_full
select fuel_type_code_pudl as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g, round(sum(gen_2010 + gen_2011 + gen_2012 + gen_2013 + gen_2014 + gen_2015 + gen_2016 + gen_2017 + gen_2018 + gen_2019 + gen_2020 + gen_2021 + gen_2022 + gen_2023 + gen_2024 + gen_2025 + gen_2026)/1000000.0, 3) as v from t group by k order by v desc limit 500

# state_2distinct
select state as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g from t group by k order by n desc limit 500

# state_full
select state as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g, round(sum(gen_2010 + gen_2011 + gen_2012 + gen_2013 + gen_2014 + gen_2015 + gen_2016 + gen_2017 + gen_2018 + gen_2019 + gen_2020 + gen_2021 + gen_2022 + gen_2023 + gen_2024 + gen_2025 + gen_2026)/1000000.0, 3) as v from t group by k order by v desc limit 500

# utility_count
select utility_name_eia as k, count(*) as n from t group by k order by n desc limit 300

# utility_2distinct
select utility_name_eia as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g from t group by k order by n desc limit 300

# utility_full
select utility_name_eia as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g, round(sum(gen_2010 + gen_2011 + gen_2012 + gen_2013 + gen_2014 + gen_2015 + gen_2016 + gen_2017 + gen_2018 + gen_2019 + gen_2020 + gen_2021 + gen_2022 + gen_2023 + gen_2024 + gen_2025 + gen_2026)/1000000.0, 3) as v from t group by k order by v desc limit 300

# totals_unfiltered
select count(distinct plant_id_eia) as plants, count(distinct gen_key) as gens from t

# tech_2distinct
select technology_description as k, count(distinct plant_id_eia) as n, count(distinct gen_key) as g from t group by k order by n desc limit 500
