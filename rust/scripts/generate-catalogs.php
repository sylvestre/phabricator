<?php

// Writes the catalogs the Rust pages and phab-cli read instead of PHP's
// classes: the applications, the configuration options and the Conduit
// methods, as JSON on stdout. Run it in the PHP container of the dev stack
// and keep its output in the crates (rust/README.md, "Catalogs"):
//
//   docker compose exec -T phabricator php /dev/stdin applications \
//     < rust/scripts/generate-catalogs.php \
//     > rust/crates/phab-web/catalog/applications.json
//
// with "applications", "config", "conduit" or "storage" as the argument.

$which = isset($argv[1]) ? $argv[1] : null;
$root = '/app/phabricator';
require_once $root.'/scripts/init/init-script.php';

function catalog_private($object, $name) {
  $class = new ReflectionClass($object);
  while (!$class->hasProperty($name)) {
    $class = $class->getParentClass();
  }
  $property = $class->getProperty($name);
  $property->setAccessible(true);
  return $property->getValue($object);
}

function catalog_string($value) {
  if ($value === null) {
    return null;
  }
  if ($value instanceof PhutilSafeHTML) {
    return $value->getHTMLContent();
  }
  return (string)$value;
}

function catalog_applications() {
  $user = new PhabricatorUser();
  $admin = id(new PhabricatorUser())->setIsAdmin(true);
  $out = array();
  $applications = PhabricatorApplication::getAllApplications();
  foreach ($applications as $application) {
    $capabilities = array();
    $method = new ReflectionMethod($application, 'getCustomCapabilities');
    $method->setAccessible(true);
    $specs = $method->invoke($application);
    foreach ($specs as $capability => $spec) {
      $capabilities[] = array(
        'key' => $capability,
        'label' => $application->getCapabilityLabel($capability),
        'default' => idx($spec, 'default', PhabricatorPolicies::POLICY_USER),
        'editable' => (bool)idx($spec, 'edit', true),
        'caption' => catalog_string(idx($spec, 'caption')),
      );
    }
    $articles = array();
    foreach ($application->getHelpDocumentationArticles($user) as $article) {
      $articles[] = array(
        'name' => catalog_string($article['name']),
        'href' => (string)$article['href'],
      );
    }
    $out[] = array(
      'class' => get_class($application),
      'name' => catalog_string($application->getName()),
      'short_description' => catalog_string(
        $application->getShortDescription()),
      'flavor_text' => catalog_string($application->getFlavorText()),
      'overview' => catalog_string($application->getOverview()),
      'icon' => $application->getIcon(),
      'base_uri' => $application->getBaseURI(),
      'group' => $application->getApplicationGroup(),
      'order' => (float)$application->getApplicationOrder(),
      'launchable' => (bool)$application->isLaunchable(),
      'unlisted' => (bool)$application->isUnlisted(),
      'prototype' => (bool)$application->isPrototype(),
      'can_uninstall' => (bool)$application->canUninstall(),
      'first_party' => (bool)$application->isFirstParty(),
      'pinned_by_default' => (bool)$application->isPinnedByDefault($user),
      'pinned_by_default_for_admins' =>
        (bool)$application->isPinnedByDefault($admin),
      'capabilities' => $capabilities,
      'help_articles' => $articles,
    );
  }
  return $out;
}

function catalog_config() {
  $out = array();
  $options = PhabricatorApplicationConfigOptions::loadAllOptions();
  ksort($options);
  foreach ($options as $key => $option) {
    $type = $option->getType();
    $enum = null;
    if ($type == 'enum') {
      $enum = array_map('catalog_string', $option->getEnumOptions());
    }
    $classes = null;
    if ($type == 'class') {
      $symbols = id(new PhutilSymbolLoader())
        ->setType('class')
        ->setAncestorClass($option->getBaseClass())
        ->setConcreteOnly(true)
        ->selectSymbolsWithoutLoading();
      $classes = array_values(ipull($symbols, 'name'));
      sort($classes);
    }
    $examples = array();
    foreach ((array)$option->getExamples() as $example) {
      $examples[] = array(
        'value' => is_array($example[0]) ? $example[0] : catalog_string($example[0]),
        'description' => catalog_string($example[1]),
      );
    }
    $group = $option->getGroup();
    $out[] = array(
      'key' => $key,
      'type' => $type,
      'default' => $option->getDefault(),
      'summary' => catalog_string($option->getSummary()),
      'description' => catalog_string($option->getDescription()),
      'locked' => (bool)catalog_private($option, 'locked'),
      'hidden' => (bool)catalog_private($option, 'hidden'),
      'locked_message' => catalog_string(
        catalog_private($option, 'lockedMessage')),
      'group' => $group ? $group->getName() : null,
      'group_key' => $group ? $group->getKey() : null,
      'bool_options' => $type == 'bool'
        ? array_map('catalog_string', $option->getBoolOptions())
        : null,
      'enum_options' => $enum,
      'classes' => $classes,
      'base_class' => $option->getBaseClass(),
      'examples' => $examples,
    );
  }
  return $out;
}

function catalog_conduit() {
  $out = array();
  $user = new PhabricatorUser();
  $methods = id(new PhutilClassMapQuery())
    ->setAncestorClass('ConduitAPIMethod')
    ->setSortMethod('getSortOrder')
    ->execute();
  foreach ($methods as $method) {
    $method->setViewer($user);
    $application = $method->getApplication();
    $errors = array();
    foreach ($method->getErrorTypes() as $code => $meaning) {
      $errors[] = array('code' => $code, 'meaning' => catalog_string($meaning));
    }
    $params = array();
    foreach ($method->getParamTypes() as $name => $type) {
      $params[] = array('name' => $name, 'type' => catalog_string($type));
    }
    try {
      $pages = count($method->getDocumentationPages($user));
    } catch (Exception $ex) {
      $pages = -1;
    }
    $out[] = array(
      'name' => $method->getAPIMethodName(),
      'application_name' => $method->getApplicationName(),
      'application' => $application ? get_class($application) : null,
      'summary' => catalog_string($method->getMethodSummary()),
      'description' => catalog_string($method->getMethodDescription()),
      'status' => $method->getMethodStatus(),
      'status_description' => catalog_string(
        $method->getMethodStatusDescription()),
      'return_type' => catalog_string($method->getReturnType()),
      'params' => $params,
      'errors' => $errors,
      'scope' => $method->getRequiredScope(),
      'internal' => (bool)$method->isInternalAPI(),
      'requires_authentication' => (bool)$method->shouldRequireAuthentication(),
      'documentation_pages' => $pages,
    );
  }
  return $out;
}

function catalog_storage() {
  $root = dirname(phutil_get_library_root('phabricator'));
  $out = array();
  foreach (PhabricatorSQLPatchList::buildAllPatches() as $patch) {
    $name = $patch->getName();
    if (!strncmp($name, $root.'/', strlen($root) + 1)) {
      $name = '{root}'.substr($name, strlen($root));
    }
    $out[] = array(
      'key' => $patch->getFullKey(),
      'phase' => $patch->getPhase(),
      'type' => $patch->getType(),
      'name' => $name,
    );
  }
  return $out;
}

switch ($which) {
  case 'applications':
    $data = catalog_applications();
    break;
  case 'config':
    $data = catalog_config();
    break;
  case 'conduit':
    $data = catalog_conduit();
    break;
  case 'storage':
    $data = catalog_storage();
    break;
  default:
    fwrite(STDERR, "usage: generate-catalogs.php applications|config|conduit|storage\n");
    exit(1);
}
echo id(new PhutilJSON())->encodeAsList($data);
