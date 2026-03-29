# Borneo

Borneo is a compact Java build tool, inspired by Cargo, made to suit simple projects. It uses a KDL-based configuration file for declaring Maven repositories and dependencies, and a lockfile that tracks dependency resolution, allowing subsequent runs to skip network access entirely. It supports Maven repositories, POMs and BOMs, string templates, property, dependency inheritance and exclusions. It supports shadowing by default, but no relocation support as of now.

Downloaded dependencies reside in a project-local directory: `./build/cache`. In contrast to Maven and Gradle, Borneo does not maintain a global repository, and discards all POM files. The lockfile is the single source of truth for transitive dependencies.

## Getting started

You must have Rust installed, currently on 1.94.0.

```bash
$ cargo install --git https://github.com/saiintbrisson/borneo
$ borneo -V
borneo 0.1.0
```

### The manifest file

Borneo uses [KDL](https://kdl.dev/) for its config file, located in the project's root, called `borneo.kdl`. Every project needs a group ID, artifact ID and a version:

```kdl
group "com.example"
artifact "my-app"
version "1.0.0"

entry "com.example.my_app.Main" // optional
```

And you can execute it with `borneo run` (or `borneo r`):

```bash
$ borneo run
compiled 1 source file
packaged build/my-app-1.0.0.jar

Hello, World!
```

By default, source and resources are located in `src/main/java` and `src/main/resources`, but can be overriden in the manifest:

```kdl
group "com.example"
artifact "my-app"
version "1.0.0"
entry "com.example.Main"
source "src/"
resources "assets/"
```

#### Repositories and dependencies

The `dependencies` node allows declaring artifacts to be searched in the Maven repositories. By default, only Maven Central is enabled.

```kdl
group "com.example"
artifact "my-app"
version "1.0.0"

dependencies {
    compile "com.google.guava:guava:33.3.1-jre" {
        exclude "com.google.code.findbugs:jsr305"
        exclude "com.google.errorprone:error_prone_annotations"
    }

    compile "com.google.code.gson:gson:2.11.0"

    runtime "org.slf4j:slf4j-simple:2.0.16"
    provided "org.apache.logging.log4j:log4j-api:2.24.1"
}
```

A dependency declaration is composed of its scope (as of now, only `compile`, `provided`, and `runtime` are supported), an artifact ID (or coordinates, G:A:V triple), and optionally children `exclude` nodes in `G:A` format.

Like the former, repositories have their own node as well and it can be used to add new sources as well as disabling the central repo:

```kdl
group "com.example"
artifact "my-plugin"
version "1.0.0"

repositories {
    central enabled="false" // disables the Maven central repository

    "https://repo.papermc.io/repository/maven-public"
}

dependencies {
    provided "io.papermc.paper:paper-api:1.21.11-R0.1-SNAPSHOT"
}
```

Running `borneo build` (or `borneo b`) will resolve all dependencies, fetching all POMs, BOMs, parents, and the whole ordeal, then de-duplicates dependencies, prioritizing by least distant from root declaration, like Maven does, and downloads them into the cache directory. It finally generates the `borneo.lock` file, describing all present dependencies computed and allowing borneo to skip this process next time it runs, if no dependencies or repositories change.

#### Build configuration

In many cases, you might want to create a fat JAR (_shadowing_ dependencies). Borneo has limited support for this operation, though relocation is still a WIP.

```kdl
group "rs.luiz"
artifact "my-plugin"
version "1.0.0"

repositories {
    "https://repo.papermc.io/repository/maven-public"
}

dependencies {
    compile "com.google.code.gson:gson:2.11.0"
    compile "org.apache.commons:commons-lang3:3.17.0"
    provided "io.papermc.paper:paper-api:1.21.11-R0.1-SNAPSHOT"
}

build {
    output "./server/plugins/my-plugin.jar"
    shadow "true"
}
```

The resulting `my-plugin.jar` will have all `compile` and `runtime` dependencies shadowed. In this case, `gson` and `commons-lang3`.

Another build option is also available, `post-build` executes a shell command after the build step, and has the `BORNEO_BUILD_OUTPUT` environment variable available:

```kdl
build {
  post-build "echo $BORNEO_BUILD_OUTPUT"
}
```
