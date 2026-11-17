xquery version "1.0-ml";
import module namespace admin = "http://marklogic.com/xdmp/admin" at "/MarkLogic/admin.xqy";
let $config := admin:get-configuration()
let $server := xdmp:server()
let $path := admin:appserver-get-url-rewriter($config, $server)
let $modules := admin:appserver-get-modules-database($config, $server)
let $root := admin:appserver-get-root($config, $server)
let $uri := concat(replace($root, "/$", ""), "/", replace($path, "^/", ""))
let $xml := if ($path eq "" or not(ends-with(lower-case($path), ".xml"))) then ""
  else if ($modules ne 0) then
    let $content := xdmp:eval('declare variable $uri external; let $doc := fn:doc($uri) return if (empty($doc)) then () else xdmp:quote($doc)',
      (xs:QName("uri"), $uri), <options xmlns="xdmp:eval"><database>{$modules}</database><transaction-mode>query</transaction-mode></options>)
    return if (exists($content)) then $content
      else if (starts-with($path, "/MarkLogic/") and admin:appserver-get-rewrite-resolves-globally($config, $server))
        then xdmp:quote(xdmp:document-get(concat("Modules", $path)))
      else fn:error(xs:QName("REWRITER-NOTFOUND"), concat("Rewriter not found in modules database: ", $uri))
  else xdmp:quote(xdmp:document-get($uri))
return object-node {
  "path": $path, "root": $root,
  "database": if ($modules eq 0) then "Filesystem" else xdmp:database-name($modules),
  "xml": if (string-length($xml) gt 8388608) then fn:error(xs:QName("REWRITER-TOOLARGE"), "Rewriter exceeds 8 MiB") else $xml,
  "message": if ($path eq "") then "This app server has no URL rewriter configured."
    else if (not(ends-with(lower-case($path), ".xml"))) then "The configured rewriter is executable code. Only XML route definitions can be listed." else ""
}
