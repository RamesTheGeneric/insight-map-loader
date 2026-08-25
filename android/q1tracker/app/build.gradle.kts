plugins { id("com.android.application") }

android {
    namespace = "com.mapperlocalizer.questtracker"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.mapperlocalizer.questtracker"
        // Quest 1 is an old Android base and is long EOL; keep the floor low.
        minSdk = 24
        // Deliberately not chasing the latest target: this is a sideloaded VR
        // app on an EOL device, and newer targets add restrictions that buy us
        // nothing here.
        targetSdk = 32
        versionCode = 1
        versionName = "1.0"
        ndk { abiFilters += "arm64-v8a" }
        externalNativeBuild { cmake { arguments += listOf("-DANDROID_STL=c++_static") } }
    }
    externalNativeBuild { cmake { path = file("src/main/cpp/CMakeLists.txt"); version = "3.22.1" } }
    buildFeatures { prefab = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildTypes { release { isMinifyEnabled = false } }
}

dependencies {
    // Provides libopenxr_loader.so + headers as a prefab module.
    implementation("org.khronos.openxr:openxr_loader_for_android:1.1.43")
}
