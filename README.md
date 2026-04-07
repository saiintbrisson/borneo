# Borneo

Borneo is a compact Java build tool, inspired by Cargo, made to suit simple projects. It's designed to be modern and slim. It supports Maven repositories and dependencies, maintains a per-project a lockfile and dependency cache. Borneo does not maintain a global repository, XML files are discarded after the lockfile is computed.

## Getting started

You must have Rust installed, 1.94.0+.

```bash
$ cargo install --git https://github.com/saiintbrisson/borneo
$ borneo -V
borneo 0.1.0
```

Use `borneo -h` to get familiar with the commands it provides. You won't need much more than `borneo run`, `borneo build`, or `borneo test`.

The `borneo clean` and `borneo clean --purge` commands are also available. The former cleans the entire build directory, while the latter purges all unused dependencies from the cache directory.

## The manifest file

Borneo uses [KDL](https://kdl.dev/) for its config file, located in the project's root, called `borneo.kdl`, conveniently similar to Gradle's build scripts. Every project requires a group ID, artifact ID and a version, all other fields are optional:

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

By default, source and resources are located in `src/main/java` and `src/main/resources`, but can be overridden in the manifest:

```kdl
group "com.example"
artifact "my-app"
version "1.0.0"
entry "com.example.Main"
source "src/"
resources "assets/"
```

### Repositories and dependencies

The `dependencies` node allows declaring artifacts to be searched in the repositories.

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

    processor "org.projectlombok:lombok:1.18.44"

    compile path="libs/my-custom.jar"
}
```

A dependency declaration is composed of its scope, an artifact ID (or coordinates, G:A:V triple), and optionally children `exclude` nodes in `G:A` format. Dependencies can also be sourced from local files using the `path` property: `compile path="path/to/jar"`. The following scopes are available:

| scope       | compile classpath         | bundled in shadow | notes                                  |
|-------------|---------------------------|-------------------|----------------------------------------|
| `compile`   | yes                       | yes               | the default scope                      |
| `runtime`   | no                        | yes               | tvailable at runtime only              |
| `provided`  | yes                       | no                | supplied by the runtime environment    |
| `processor` | annotation processor path | no                | for code generation tools like Lombok  |
| `test`      | test classpath only       | no                | only available during `borneo test`    |

Like for dependencies, repository sources can be added and customized:

```kdl
group "com.example"
artifact "my-plugin"
version "1.0.0"

repositories {
    central enabled="false" // disables the Maven central repository
    "https://repo.papermc.io/repository/maven-public" checksum-policy="warn"
    "central.sonatype.com/repository/maven-snapshots"
}

dependencies {
    provided "io.papermc.paper:paper-api:1.21.11-R0.1-SNAPSHOT"
}
```

The order in which repositories are declared dictates how the resolver selects results. If an artifact exists in multiple repositories, the first declared repository wins. By default, the Maven Central repository is implicitly declared at highest priority, you can change this:

```kdl
repositories {
    "central.sonatype.com/repository/maven-snapshots"
    central
}
```

Different checksum policies exist for repositories:

| policy           | missing checksum | mismatched checksum |
|------------------|------------------|---------------------|
| `required`       | error            | error               |
| `fail` (default) | ok               | error               |
| `warn`           | ok               | warning             |
| `ignore`         | ok               | ok                  |

The default strategy for searches is to race all repository look-ups. It may be desired to have the search be sequential, use the `strategy` property to choose your preferred strategy:

```kdl
repositories strategy="sequential" { ... }
```

Running `borneo build` (or `borneo b`) will resolve all dependencies, fetching all POMs, BOMs, parents, and the whole ordeal, then de-duplicates dependencies, prioritizing by least distant from root declaration, like Maven does, and downloads them into the cache directory. It finally generates the `borneo.lock` file, describing all present dependencies computed and allowing borneo to skip this process next time it runs, if no dependencies or repositories change.

### Build configuration

When left untouched, your built artifacts will be located in `build/my-artifact-0.1.0.jar`, but sometimes you need a little bit more. The `build` node allows you to customize how your artifact is built.

```kdl
group "rs.luiz"
artifact "my-plugin"
version "1.0.0"

dependencies {
    compile "org.apache.commons:commons-lang3:3.17.0"
}

build {
    output "./path/to/my.jar"
    shadow "true"

    post-build "echo $BORNEO_BUILD_OUTPUT"

    manifest {
        Implementation-Title "My App"
        Implementation-Version "1.0.0"
        Built-By "borneo"
    }
}
```

These are all the relevant build options. `manifest` is written as a K-V list of entries to be written to `META-INF/MANIFEST.MF`. `shadow` toggles bundling of dependencies into your final JAR, if no `output` is declared, it will be located in `build/my-artifact-0.1.0-all.jar`. `post-build` executes a shell command after the build step, and has the `BORNEO_BUILD_OUTPUT` environment variable available.

### Java node

A `java` node is available. It allows you to declare the minimum supported Java release and common compiler arguments you might want to pass to `javac` invocations:

```kdl
java {
    release "21"
    compiler-args "-Xlint:deprecation" "-Xlint:unchecked"
}
```

If the `JAVA_HOME` version is older than the one in `release`, Borneo will raise an error.

### Testing

Testing is in... a rough shape. It is somewhat supported if you run more modern testing setups. For the `borneo test` command to be available, you must declare `org.junit.platform:junit-platform-console-standalone` as a test dependency. I suggest you take a look at how it works. In most cases, it will be able to run your test suite regardless of what framework you use (JUnit, TestNG, etc). A node is also available for limited customization:

```kdl
test {
    source "src/test/java"
    resources "src/test/resources"
    jvm-args "-Xmx512m" "-ea"
}
```

> `source` and `resources` are optional fields.

