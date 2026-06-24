import org.gradle.internal.os.OperatingSystem

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "io.github.steeb_k.seedsync"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.steeb_k.seedsync"
        // MANAGE_EXTERNAL_STORAGE (All-Files Access) is API 30+, so we target the
        // modern scoped-storage branch and skip the legacy WRITE_EXTERNAL_STORAGE path.
        minSdk = 30
        targetSdk = 35
        versionCode = 1
        versionName = "1.1.2"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    // The Rust .so files are produced by cargo-ndk into src/main/jniLibs.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
    // UniFFI-generated Kotlin is written into the default source root
    // (src/main/java/uniffi/...) by the uniffiBindgen task, so it compiles like
    // any other source without dynamic generated-source registration.

    packaging {
        // iroh/quinn ship a single .so each; nothing to dedupe, but keep licenses tidy.
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-service:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")

    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")

    implementation("androidx.work:work-runtime-ktx:2.10.0")

    // Required at runtime by the UniFFI-generated Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.15.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    // Kotlin support class that the Rust rustls-platform-verifier calls back into
    // for Android trust-store validation (iroh relay TLS). Resolved from the
    // local Maven repo wired up in settings.gradle.kts.
    implementation("rustls:rustls-platform-verifier:0.1.1")

    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("androidx.compose.ui:ui-tooling-preview")
}

// ---------------------------------------------------------------------------
// Rust build: compile crates/seed-mobile to per-ABI .so via cargo-ndk, then
// generate the UniFFI Kotlin bindings from the built library.
// ---------------------------------------------------------------------------

val workspaceRoot = rootProject.projectDir.parentFile // android/ -> repo root
val jniLibsDir = file("src/main/jniLibs")
// Generated UniFFI Kotlin is written under the default source root so it
// compiles without dynamic generated-source registration (which AGP+Kotlin
// wire inconsistently). The uniffi/ subtree is gitignored — it is regenerated
// by the uniffiBindgen task on every build.
val uniffiOutDir = layout.projectDirectory.dir("src/main/java")

// ABIs to build, mapped to their Rust target triples. arm64 is primary (modern
// phones); armeabi-v7a for older 32-bit devices; x86_64 for the emulator.
val abiToTriple = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86_64" to "x86_64-linux-android",
)

fun cargo(vararg args: String): List<String> =
    if (OperatingSystem.current().isWindows)
        listOf("cmd", "/c", "cargo", *args)
    else
        listOf("cargo", *args)

// Build (without cargo-ndk's `-o`, which over-copies intermediate dependency
// dylibs from deps/) then copy only the self-contained libseed_mobile.so per ABI.
val cargoNdkBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Cross-compile seed-mobile to Android .so via cargo-ndk"
    workingDir = workspaceRoot
    // NDK location: resolved from ANDROID_NDK_HOME / a gradle property; cargo-ndk
    // also auto-detects the SDK's ndk/ dir when ANDROID_HOME is set.
    val ndk = (findProperty("ndkDir") as String?) ?: System.getenv("ANDROID_NDK_HOME")
    if (ndk != null) environment("ANDROID_NDK_HOME", ndk)

    val abiArgs = abiToTriple.keys.flatMap { listOf("-t", it) }
    commandLine(
        cargo(
            "ndk",
            *abiArgs.toTypedArray(),
            "build", "--release", "-p", "seed-mobile"
        )
    )
    doLast {
        abiToTriple.forEach { (abi, triple) ->
            val built = File(workspaceRoot, "target/$triple/release/libseed_mobile.so")
            val destDir = File(jniLibsDir, abi).apply { mkdirs() }
            built.copyTo(File(destDir, "libseed_mobile.so"), overwrite = true)
        }
    }
}

// Build the host cdylib so the binding generator can read it. UniFFI's
// `--library` metadata extraction can't parse a foreign-arch (Android) .so on a
// Windows host, and UniFFI metadata is platform-independent anyway — the Kotlin
// generated from the host library is identical to what the Android .so would
// yield, and the runtime checksum guards still match since both are built from
// the same source + UniFFI version.
val cargoHostBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Build the host seed-mobile cdylib for UniFFI binding generation"
    workingDir = workspaceRoot
    commandLine(cargo("build", "-p", "seed-mobile", "--lib"))
}

val hostLibName = when {
    OperatingSystem.current().isWindows -> "seed_mobile.dll"
    OperatingSystem.current().isMacOsX -> "libseed_mobile.dylib"
    else -> "libseed_mobile.so"
}

val uniffiBindgen by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generate Kotlin UniFFI bindings from the host seed-mobile library"
    dependsOn(cargoHostBuild)
    workingDir = workspaceRoot
    val hostLib = File(workspaceRoot, "target/debug/$hostLibName")
    val outDir = uniffiOutDir.asFile
    doFirst { outDir.mkdirs() }
    commandLine(
        cargo(
            "run", "-p", "seed-mobile", "--features", "bindgen", "--bin", "uniffi-bindgen", "--",
            "generate", "--library", hostLib.absolutePath,
            "--language", "kotlin", "--out-dir", outDir.absolutePath
        )
    )
}

// Wire the Rust build ahead of Kotlin compilation/packaging: bindgen produces
// the Kotlin sources, cargoNdkBuild produces the per-ABI .so into jniLibs.
tasks.named("preBuild") {
    dependsOn(uniffiBindgen, cargoNdkBuild)
}
