xquery version "1.0-ml";
object-node {
  "local": xdmp:host-name(xdmp:host()),
  "now": fn:string(fn:current-dateTime()),
  "hosts": array-node { for $host in xdmp:hosts() return xdmp:host-name($host) }
}
