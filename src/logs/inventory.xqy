xquery version "1.0-ml";
declare variable $expected as xs:string external;
declare variable $directory as xs:string external;
declare variable $days-ago as xs:string external;
declare variable $show-untracked as xs:string external;
declare variable $tracked-names as xs:string external;

declare function local:path($path as xs:string) as xs:string {
  let $path := fn:replace($path, "^file://[^/]+", "")
  return if (fn:starts-with($path, "/")) then $path else "/" || $path
};
let $host := $expected
let $dir := if ($directory eq "") then xdmp:data-directory() || "/Logs" else $directory
let $selected-days-ago := xs:integer($days-ago)
let $names := xdmp:to-json(xdmp:from-json-string($tracked-names))/array-node()/node()
return
  if (not($host = (xdmp:hosts() ! xdmp:host-name(.)))) then
    fn:error(xs:QName("HOST-MISMATCH"), "Not a cluster host: " || $host)
  else object-node {
    "host": $host, "directory": $dir, "inventory": false(),
    "read_bytes": 0, "cluster_bytes": 0, "chunks": array-node {},
    "files": array-node {
      for $entry in xdmp:filesystem-directory("file://" || $host || local:path($dir))/*:entry
        [*:type eq "file"]
        [fn:matches(*:filename, "(Error|Access|Request|Audit)Log(_[0-9]+)?\.txt$")]
      let $name := xs:string($entry/*:filename)
      let $rotation := if (fn:matches($name, "_[0-9]+\.txt$"))
                       then xs:integer(fn:replace($name, "^.*_([0-9]+)\.txt$", "$1"))
                       else 0
      where ($rotation = $selected-days-ago and $show-untracked eq "true")
        or ($rotation = $selected-days-ago and $name = ($names ! xs:string(.)))
      order by xs:string($entry/*:filename)
      return object-node {
        "host": $host, "path": local:path(xs:string($entry/*:pathname)),
        "name": xs:string($entry/*:filename),
        "size": xs:unsignedLong($entry/*:content-length),
        "modified": xs:string($entry/*:last-modified)
      }
    }
  }
