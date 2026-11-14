xquery version "1.0-ml";

declare variable $LIMIT as xs:string external;

(: Change the query to select the documents for this job. :)
let $uris := cts:uris((), (), cts:true-query())[1 to xs:integer($LIMIT)]
return (count($uris), $uris)
