xquery version "1.0-ml";
declare variable $expected as xs:string external;
declare variable $directory as xs:string external;
declare variable $name as xs:string external;
declare variable $start as xs:string external;
declare variable $limit as xs:string external;

(: Time-window read of one error-log stream. xdmp:logfile-scan filters records
   by timestamp on the server, so only matching records cross the network. The
   stream's rotated archives (ErrorLog_1.txt ...) are included when they can
   hold records in the window, oldest first, so a midnight rotation between two
   polls loses nothing. :)
declare function local:path($path as xs:string) as xs:string {
  let $path := fn:replace($path, "^file://[^/]+", "")
  return if (fn:starts-with($path, "/")) then $path else "/" || $path
};
let $host := $expected
let $max := xs:integer($limit)
let $dir := if ($directory eq "") then xdmp:data-directory() || "/Logs" else $directory
return
  if (not($host = (xdmp:hosts() ! xdmp:host-name(.)))) then fn:error(xs:QName("HOST-MISMATCH"), "Not a cluster host: " || $host)
  else if (not(fn:matches($name, "^[A-Za-z0-9_]+\.txt$"))) then fn:error(xs:QName("BAD-LOG-NAME"), "Unsupported log name: " || $name)
  else
    let $stem := fn:replace($name, "\.txt$", "")
    let $from := xs:dateTime($start)
    let $entries := xdmp:filesystem-directory("file://" || $host || local:path($dir))/*:entry
      [*:type eq "file"][fn:matches(*:filename, "^" || $stem || "(_[0-9]+)?\.txt$")]
      [xs:string(*:filename) eq $name or fn:substring(xs:string(*:last-modified), 1, 19) ge fn:substring($start, 1, 19)]
    let $current := $entries[xs:string(*:filename) eq $name][1]
    let $ordered :=
      for $entry in $entries
      order by (if (xs:string($entry/*:filename) eq $name) then 0
                else xs:integer(fn:replace($entry/*:filename, "^.*_([0-9]+)\.txt$", "$1"))) descending
      return $entry
    let $records :=
      for $entry in $ordered
      return xdmp:logfile-scan("file://" || $host || local:path(xs:string($entry/*:pathname)), (), (), $from, (), $max)
    return object-node {
      "host": $host,
      "size": if (fn:exists($current)) then xs:unsignedLong($current/*:content-length) else (),
      "modified": if (fn:exists($current)) then xs:string($current/*:last-modified) else (),
      "lines": array-node { fn:subsequence($records, 1, $max) }
    }
