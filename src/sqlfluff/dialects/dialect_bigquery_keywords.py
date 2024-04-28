"""A list of all BigQuery SQL key words."""

# https://cloud.google.com/bigquery/docs/reference/standard-sql/lexical#reserved_keywords
bigquery_reserved_keywords = """ALL
AND
ANY
ARRAY
AS
ASC
ASSERT_ROWS_MODIFIED
AT
BETWEEN
BY
CASE
CAST
CLONE
COLLATE
CONTAINS
CREATE
CROSS
CUBE
CURRENT
DEFAULT
DEFINE
DESC
DISTINCT
ELSE
END
ENUM
ESCAPE
EXCEPT
EXCLUDE
EXISTS
FALSE
FETCH
FOLLOWING
FOR
FROM
FULL
GROUP
GROUPING
GROUPS
HASH
HAVING
IF
IGNORE
IN
INCLUDE
INNER
INTERSECT
INTERVAL
INTO
IS
JOIN
LATERAL
LEFT
LIKE
LIMIT
LOOKUP
MERGE
NEW
NO
NOT
NULL
NULLS
OF
ON
OR
ORDER
OUTER
OVER
PARTITION
PIVOT
PRECEDING
PRIMARY
PROTO
RANGE
RECURSIVE
RESPECT
RIGHT
ROLLUP
ROWS
SELECT
SET
SOME
STRUCT
TABLESAMPLE
THEN
TO
TREAT
TRUE
UNBOUNDED
UNION
UNNEST
UNPIVOT
USING
WHEN
WHERE
WINDOW
WITH
WITHIN"""

# Note BigQuery doesn't have a list of Unreserved Keywords
# so these are just ones we need to allow parsing to work
bigquery_unreserved_keywords = """ACCESS
ACCOUNT
ADD
ADMIN
AFTER
ALTER
APPLY
ASSERT
AUTO_INCREMENT
BEGIN
BERNOULLI
BINARY
BINDING
BREAK
CACHE
CALL
CASCADE
CHAIN
CHARACTER
CHECK
CLONE
CLUSTER
COLUMN
COLUMNS
COMMENT
COMMIT
CONCURRENTLY
CONTINUE
CONNECT
CONNECTION
CONSTRAINT
COPY
CURRENT_USER
CYCLE
DATA
DATABASE
DATE
DATETIME
DECLARE
DELETE
DESCRIBE
DETERMINISTIC
DO
DOMAIN
DOUBLE
DROP
ELSEIF
ENFORCED
ERROR
EXCEPTION
EXECUTE
EXECUTION
EXPLAIN
EXPORT
EXTENSION
EXTERNAL
FILE
FILTER
FIRST
FOREIGN
FORMAT
FRIDAY
FUNCTION
FUTURE
GRANT
GRANTED
GRANTS
HOUR
ILIKE
IMMEDIATE
IMPORTED
IN
INCREMENT
INDEX
INOUT
INSERT
INTEGRATION
KEY
ITERATE
LANGUAGE
LARGE
LAST
LEAVE
LOOP
MANAGE
MASKING
MATCHED
MATERIALIZED
MAX
MAXVALUE
MESSAGE
MIN
MINUS
MINVALUE
ML
MODEL
MODIFY
MONDAY
MONITOR
NAME
NAN
NFC
NFKC
NFD
NFKD
NOCACHE
NOCYCLE
NOORDER
OBJECT
OFFSET
OPERATE
OPTION
OPTIONS
ORDINAL
OUT
OVERLAPS
OVERWRITE
OWNERSHIP
PERCENT
PIPE
POLICY
PRECISION
PRIMARY
PRIOR
PRIVILEGES
PROCEDURE
PUBLIC
QUALIFY
QUARTER
RAISE
READ
REFERENCE_USAGE
REFERENCES
RENAME
REPEAT
REPEATABLE
REPLACE
REPLICA
RESOURCE
RESTRICT
RETURN
RETURNS
REVOKE
RLIKE
ROLE
ROLLBACK
ROW
ROUTINE
SAFE
SATURDAY
SCHEMA
SCHEMAS
SECOND
SEPARATOR
SERVER
SEQUENCE
SESSION_USER
SHARE
SNAPSHOT
SOURCE
STAGE
START
STREAM
SUNDAY
SYSTEM
SYSTEM_TIME
TABLE
TABLESPACE
TARGET
TASK
TEMP
TEMPORARY
THURSDAY
TIME
TIMESTAMP
TRANSACTION
TRANSIENT
TRIGGER
TRUNCATE
TUESDAY
TYPE
UNIQUE
UNSIGNED
UNTIL
UPDATE
USAGE
USE
USE_ANY_ROLE
USER
VALUE
VALUES
VARYING
VERSION
VIEW
WAREHOUSE
WEDNESDAY
WEEK
WHILE
WITHOUT
WORK
WRAPPER
WRITE
ZONE"""
