xquery version "1.0-ml";
declare variable $expected as xs:string external;
declare variable $directory as xs:string external;
declare variable $requests as xs:string external;
declare variable $modified-since as xs:string external;

(: Every filesystem operation names its target host via the cluster file
   protocol. The client only needs access to the configured eval endpoint. :)
declare function local:path($path as xs:string) as xs:string {
  let $path := fn:replace($path, "^file://[^/]+", "")
  return if (fn:starts-with($path, "/")) then $path else "/" || $path
};
let $host := $expected
let $dir := if ($directory eq "") then xdmp:data-directory() || "/Logs" else $directory
return
  if (not($host = (xdmp:hosts() ! xdmp:host-name(.)))) then fn:error(xs:QName("HOST-MISMATCH"), "Not a cluster host: " || $host)
  else
    let $entries := xdmp:filesystem-directory("file://" || $host || local:path($dir))/*:entry
      [*:type eq "file"][fn:matches(*:filename, "(Error|Access|Request|Audit)Log(_[0-9]+)?\.txt$")]
      [$modified-since eq "" or xs:dateTime(*:last-modified) ge xs:dateTime($modified-since)]
    let $wanted := xdmp:to-json(xdmp:from-json-string($requests))/array-node()
    let $limit := xs:integer(fn:min((65536, fn:max((1, fn:floor(524288 div fn:max((1, fn:count($wanted/node())))))))))
    let $read-bytes := fn:sum(
      for $request in $wanted/node()
      let $entry := $entries[local:path(xs:string(*:pathname)) eq xs:string($request/path)]
      where not($request/full = false() and $request/complete = true()
        and $request/known_size = $entry/*:content-length
        and $request/known_modified = $entry/*:last-modified)
      return xs:unsignedLong($entry/*:content-length)
    )
    return object-node {
      "host": $host,
      "read_bytes": $read-bytes,
      "cluster_bytes": if ($host ne xdmp:host-name(xdmp:host())) then $read-bytes else 0,
      "inventory": empty($wanted/node()),
      "directory": $dir,
      "files": array-node {
        for $entry in $entries
        where empty($wanted/node()) or local:path(xs:string($entry/*:pathname)) = $wanted/node()/path
        order by xs:string($entry/*:filename)
        return object-node {
          "host": $host, "path": local:path(xs:string($entry/*:pathname)),
          "name": xs:string($entry/*:filename),
          "size": xs:unsignedLong($entry/*:content-length),
          "modified": xs:string($entry/*:last-modified)
        }
      },
      "chunks": array-node {
        for $request in $wanted/node()
        let $path := xs:string($request/path)
        let $entry := $entries[local:path(xs:string(*:pathname)) eq $path]
        return try {
          if (empty($entry)) then fn:error(xs:QName("LOG-MISSING"), "Log disappeared: " || $path)
          else if ($request/full = false() and $request/complete = true()
              and $request/known_size = $entry/*:content-length
              and $request/known_modified = $entry/*:last-modified) then ()
          else
            (: filesystem-file reads the complete UTF-8 file on the server.
               Only the bounded suffix is sent to the client. No server fields,
               exported documents, or external-binary cache references are used. :)
            let $data := xdmp:filesystem-file("file://" || $host || $path)
            let $length := fn:string-length($data)
            let $offset := xs:integer($request/offset)
            let $anchor := xs:string($request/anchor)
            let $anchor-start := xs:integer($request/anchor_start)
            let $reset := $offset lt 0 or $length lt $offset or
              fn:substring($data, $anchor-start + 1, fn:string-length($anchor)) ne $anchor
            let $start := if ($request/full = true()) then 0
              else if ($reset) then fn:max((0, $length - $limit)) else $offset
            let $end := if ($request/full = true()) then $length else fn:min(($length, $start + $limit))
            return object-node {
              "host": $host, "path": $path,
              "start": $start, "end": $end, "length": $length,
              "reset": $reset or $request/full = true(),
              "text": fn:substring($data, $start + 1, $end - $start),
              "anchor_start": fn:max((0, $end - 256)),
              "anchor": fn:substring($data, fn:max((0, $end - 256)) + 1, fn:min((256, $end)))
            }
        } catch ($e) {
          object-node { "host": $host, "path": $path, "error": xs:string($e/*:message) }
        }
      }
    }
