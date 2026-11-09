create or replace view example as
select smthng
from smwhr
/

comment on table example is 'abc'
/

create or replace public synonym example for example
/

-- A `/` sharing its line with code is division, even at the start or end of a line
create or replace view example_division as
select 1 / 100 as z, nvl(bytes, 0) / 1024 / 1024 as size_mb
from smwhr
where sample_time > sysdate - 1/24
/

create or replace view example_division_eol as
select 1 /
100 as z
from smwhr
/

create or replace view example_division_sol as
select 1
/ 100 as z
from smwhr
/

-- The buffer executor may be indented
select smthng
from smwhr
  /

-- Plain queries and CTAS are ended by the buffer executor too
select smthng / 2
from smwhr
/

create table example_ctas as
select smthng / 2 as half
from smwhr
/
