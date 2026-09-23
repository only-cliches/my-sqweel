//! MariaDB 10.11.7 SQL_FUNCTIONS coverage manifest.
//! Every catalog name is classified as implemented or intentionally out of scope.

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FunctionScopeEntry {
    pub name: &'static str,
    pub status: &'static str,
    pub rationale: &'static str,
}
#[allow(dead_code)]
pub(crate) const MARIADB_FUNCTION_SCOPE: &[FunctionScopeEntry] = &[
    FunctionScopeEntry {
        name: "ABS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ACOS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ADDDATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ADDTIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ADD_MONTHS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ASIN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ATAN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ATAN2",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIT_AND",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIT_COUNT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIT_LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIT_OR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "BIT_XOR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CAST",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CEIL",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CEILING",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CHARACTER_LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CHAR_LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CHR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "COALESCE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONCAT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONCAT_OPERATOR_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONCAT_WS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONNECTION_ID",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONV",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CONVERT_TZ",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "COS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "COT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "COUNT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CRC32",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CRC32C",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CUME_DIST",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CURDATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "CURTIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DATABASE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DATEDIFF",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DATE_ADD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DATE_FORMAT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DATE_SUB",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DAYNAME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DAYOFMONTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DAYOFWEEK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DAYOFYEAR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DEGREES",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "DENSE_RANK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ELT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "EXP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "EXPORT_SET",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "EXTRACT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "EXTRACTVALUE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FIELD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FIND_IN_SET",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FIRST_VALUE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FLOOR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FORMAT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FROM_BASE64",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FROM_DAYS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "FROM_UNIXTIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "GET_LOCK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "GREATEST",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "GROUP_CONCAT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "HEX",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "IFNULL",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "INSTR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ISNULL",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_ARRAY",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_ARRAYAGG",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_ARRAY_APPEND",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_ARRAY_INSERT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_COMPACT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_CONTAINS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_CONTAINS_PATH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_DEPTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_DETAILED",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_EQUALS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_EXISTS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_EXTRACT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_INSERT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_KEYS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_MERGE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_MERGE_PATCH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_MERGE_PRESERVE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_NORMALIZE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_OBJECT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_OBJECTAGG",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_OVERLAPS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_PRETTY",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_QUERY",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_QUOTE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_REMOVE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_REPLACE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_SEARCH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_SET",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_TYPE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_UNQUOTE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_VALID",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "JSON_VALUE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LAG",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LAST_DAY",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LAST_INSERT_ID",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LCASE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LEAD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LEAST",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LENGTHB",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LOCATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LOG",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LOG10",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LOG2",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LOWER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LPAD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LPAD_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LTRIM",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "LTRIM_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MAKEDATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MAKETIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MAKE_SET",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MAX",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MD5",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MEDIAN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MICROSECOND",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MID",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MIN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MOD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "MONTHNAME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NOW",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NTH_VALUE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NTILE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NULLIF",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NVL",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "NVL2",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "OCT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "OCTET_LENGTH",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ORD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "PERCENT_RANK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "PERIOD_ADD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "PERIOD_DIFF",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "PI",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "POSITION",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "POW",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "POWER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "QUARTER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "QUOTE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RADIANS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RAND",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RANK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "REGEXP_INSTR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "REGEXP_REPLACE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "REGEXP_SUBSTR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RELEASE_LOCK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "REPLACE_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "REVERSE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "ROUND",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RPAD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RPAD_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RTRIM",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "RTRIM_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SCHEMA",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SEC_TO_TIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SESSION_USER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SHA",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SHA1",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SHA2",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SIGN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SIN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SOUNDEX",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SPACE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SQRT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STD",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STDDEV",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STDDEV_POP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STDDEV_SAMP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STRCMP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "STR_TO_DATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBDATE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBSTR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBSTRING",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBSTRING_INDEX",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBSTR_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUBTIME",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SUM",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "SYSTEM_USER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TAN",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TIMEDIFF",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TIME_FORMAT",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TIME_TO_SEC",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TO_BASE64",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TO_CHAR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TO_DAYS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TO_SECONDS",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TRIM",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "TRIM_ORACLE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "UCASE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "UNHEX",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "UNIX_TIMESTAMP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "UPPER",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "VARIANCE",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "VAR_POP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "VAR_SAMP",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "VERSION",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "WEEK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "WEEKDAY",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "WEEKOFYEAR",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "YEARWEEK",
        status: "implemented",
        rationale: "deterministic evaluator or query-engine support",
    },
    FunctionScopeEntry {
        name: "AES_DECRYPT",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "AES_ENCRYPT",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "BENCHMARK",
        status: "out-of-scope",
        rationale: "side-effect or timing behavior is intentionally excluded from deterministic differential coverage",
    },
    FunctionScopeEntry {
        name: "BINLOG_GTID_POS",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "COERCIBILITY",
        status: "out-of-scope",
        rationale: "MariaDB catalog entry is not a scalar function supported by the engine evaluator",
    },
    FunctionScopeEntry {
        name: "COLLATION",
        status: "out-of-scope",
        rationale: "MariaDB catalog entry is not a scalar function supported by the engine evaluator",
    },
    FunctionScopeEntry {
        name: "COLUMN_CHECK",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "COLUMN_EXISTS",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "COLUMN_JSON",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "COLUMN_LIST",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "COMPRESS",
        status: "out-of-scope",
        rationale: "MariaDB catalog entry is not a scalar function supported by the engine evaluator",
    },
    FunctionScopeEntry {
        name: "DECODE",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "DECODE_HISTOGRAM",
        status: "out-of-scope",
        rationale: "specialized Oracle/optimizer compatibility semantics are not in the deterministic core scope",
    },
    FunctionScopeEntry {
        name: "DECODE_ORACLE",
        status: "out-of-scope",
        rationale: "specialized Oracle/optimizer compatibility semantics are not in the deterministic core scope",
    },
    FunctionScopeEntry {
        name: "DES_DECRYPT",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "DES_ENCRYPT",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "ENCODE",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "ENCRYPT",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "FOUND_ROWS",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "IS_FREE_LOCK",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "IS_USED_LOCK",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "JSON_LOOSE",
        status: "out-of-scope",
        rationale: "specialized document/compatibility semantics are not implemented in the current engine",
    },
    FunctionScopeEntry {
        name: "LOAD_FILE",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "MASTER_GTID_WAIT",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "MASTER_POS_WAIT",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "NAME_CONST",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "NATURAL_SORT_KEY",
        status: "out-of-scope",
        rationale: "specialized Oracle/optimizer compatibility semantics are not in the deterministic core scope",
    },
    FunctionScopeEntry {
        name: "OLD_PASSWORD",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "PASSWORD",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "PERCENTILE_CONT",
        status: "out-of-scope",
        rationale: "requires dedicated parser/AST support not covered by scalar evaluation",
    },
    FunctionScopeEntry {
        name: "PERCENTILE_DISC",
        status: "out-of-scope",
        rationale: "requires dedicated parser/AST support not covered by scalar evaluation",
    },
    FunctionScopeEntry {
        name: "RANDOM_BYTES",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "RELEASE_ALL_LOCKS",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "ROW_COUNT",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "SCHEMAS",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "SFORMAT",
        status: "out-of-scope",
        rationale: "specialized Oracle/optimizer compatibility semantics are not in the deterministic core scope",
    },
    FunctionScopeEntry {
        name: "SLEEP",
        status: "out-of-scope",
        rationale: "side-effect or timing behavior is intentionally excluded from deterministic differential coverage",
    },
    FunctionScopeEntry {
        name: "UNCOMPRESS",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "UNCOMPRESSED_LENGTH",
        status: "out-of-scope",
        rationale: "cryptographic/legacy compatibility is not deterministic in the current engine",
    },
    FunctionScopeEntry {
        name: "UPDATEXML",
        status: "out-of-scope",
        rationale: "specialized document/compatibility semantics are not implemented in the current engine",
    },
    FunctionScopeEntry {
        name: "UUID_SHORT",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "WSREP_LAST_SEEN_GTID",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "WSREP_LAST_WRITTEN_GTID",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
    FunctionScopeEntry {
        name: "WSREP_SYNC_WAIT_UPTO_GTID",
        status: "out-of-scope",
        rationale: "server state, lock, replication, or filesystem behavior is outside query evaluation",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_has_unique_names_and_covers_catalog() {
        let mut names = MARIADB_FUNCTION_SCOPE
            .iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert!(names.windows(2).all(|pair| pair[0] != pair[1]));
        assert_eq!(names.len(), 251);
        assert!(
            MARIADB_FUNCTION_SCOPE
                .iter()
                .all(|entry| matches!(entry.status, "implemented" | "out-of-scope"))
        );
    }
}
