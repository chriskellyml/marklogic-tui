xquery version "1.0-ml";

declare variable $URI as xs:string external;
declare variable $DRY-RUN as xs:string external;

(: Start by reporting each URI. Add your transformation to the wet-run branch. :)
if ($DRY-RUN = "true") then
  concat("Would process ", $URI)
else
  concat("Processed ", $URI)
