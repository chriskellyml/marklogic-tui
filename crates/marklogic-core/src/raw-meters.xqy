xquery version "1.0-ml";

array-node {
  for $uri in cts:uri-match("*-raw.xml", ("document", "descending", "limit=16", "lazy"))
  let $doc := fn:doc($uri)
  where fn:exists($doc/*)
  let $xml := xdmp:quote($doc)
  return object-node { "uri": $uri, "xml": $xml }
}
