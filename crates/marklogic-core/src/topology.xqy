xquery version "1.0-ml";
import module namespace admin = "http://marklogic.com/xdmp/admin" at "/MarkLogic/admin.xqy";

(: A projection of one configuration: no writes, naming heuristics or per-item
   HTTP calls. String IDs preserve all 64 bits in the desktop's JavaScript. :)
let $config := admin:get-configuration()
return object-node {
  "capturedAt": fn:string(fn:current-dateTime()),
  "groups": array-node {
    for $id in admin:get-group-ids($config)
    return object-node { "id": fn:string($id), "name": admin:group-get-name($config, $id) }
  },
  "hosts": array-node {
    for $id in admin:get-host-ids($config)
    return object-node {
      "id": fn:string($id), "name": admin:host-get-name($config, $id),
      "groupId": fn:string(admin:host-get-group($config, $id))
    }
  },
  "appServers": array-node {
    for $id in admin:get-appserver-ids($config)
    let $kind := admin:appserver-get-type($config, $id)
    return object-node {
      "id": fn:string($id), "name": admin:appserver-get-name($config, $id),
      "groupId": fn:string(admin:appserver-get-group-id($config, $id)),
      "hostIds": array-node { for $host in admin:appserver-get-host-ids($config, $id) return fn:string($host) },
      "kind": $kind,
      "port": admin:appserver-get-port($config, $id),
      "secure": admin:appserver-get-ssl-certificate-template($config, $id) ne 0,
      "enabled": admin:appserver-get-enabled($config, $id),
      "contentDatabaseId": fn:string(admin:appserver-get-database($config, $id)),
      "modulesDatabaseId": if (fn:lower-case($kind) eq "webdav") then "0"
                           else fn:string(admin:appserver-get-modules-database($config, $id)),
      "root": admin:appserver-get-root($config, $id),
      "authentication": admin:appserver-get-authentication($config, $id)
    }
  },
  "databases": array-node {
    for $id in admin:get-database-ids($config)
    return object-node {
      "id": fn:string($id), "name": admin:database-get-name($config, $id),
      "forestIds": array-node { for $forest in admin:database-get-attached-forests($config, $id) return fn:string($forest) },
      "securityDatabaseId": fn:string(admin:database-get-security-database($config, $id)),
      "schemaDatabaseId": fn:string(admin:database-get-schema-database($config, $id)),
      "triggersDatabaseId": fn:string(admin:database-get-triggers-database($config, $id))
    }
  },
  "forests": array-node {
    for $id in admin:get-forest-ids($config)
    return object-node {
      "id": fn:string($id), "name": admin:forest-get-name($config, $id),
      "hostId": fn:string(admin:forest-get-host($config, $id)),
      "replicaIds": array-node { for $replica in admin:forest-get-replicas($config, $id) return fn:string($replica) },
      "failoverHostIds": array-node { for $host in admin:forest-get-failover-hosts($config, $id) return fn:string($host) },
      "dataDirectory": admin:forest-get-data-directory($config, $id),
      "enabled": admin:forest-get-enabled($config, $id)
    }
  }
}
